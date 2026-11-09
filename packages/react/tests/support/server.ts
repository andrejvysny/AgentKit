/**
 * A real HTTP server for the hooks to talk to: the Rust `http_fixture`.
 *
 * These hooks are a thin layer over `@agentkit/client`, and the interesting
 * half of what they do — optimistic record, streamed delta, terminal event,
 * reconcile — is only interesting against a server that really streams: a
 * mocked client handing back a pre-built array would test the array. The
 * fixture binary is the only backend; scripted providers, tool contributors,
 * task-store peeks and proposal staging cross its private test-control
 * listener — see `./rust-server.ts`.
 */
import type { ProposalService, ToolSetContributor } from "@agentkit/host";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import type { FetchLike } from "@agentkit/client";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type RunStatusDto,
} from "@agentkit/contracts";
import {
  createTestEventStamper,
  MockProviderClient,
  nowIso,
} from "@agentkit/testing";
import type {
  TestSecretStore,
  TestTaskStore,
} from "../../../client/tests/support/server.js";
import { startRustReactServer } from "./rust-server.js";

export const TEST_CHAT_ID = "chat-react";

export interface TestServer {
  baseUrl: string;
  store: TestTaskStore;
  provider: AiProviderClient;
  /** The staged-write pipeline the decision routes need. */
  proposals: ProposalService;
  /** Where a write-only `apiKey` lands — proof it went there and nowhere else. */
  secrets: TestSecretStore;
  stop(): Promise<void>;
}

export interface TestServerOptions {
  provider?: AiProviderClient;
  /**
   * Tools to stage for every turn. Only the multi-pass test needs them: the
   * host's chat-only retry runs when a pass FAILED and tools were staged, which
   * is the shape it exists to recover from.
   */
  contributors?: ToolSetContributor[];
  /** Gateway-side `authenticate` override. */
  deps?: {
    authenticate?: (request: Request) => Promise<unknown> | unknown;
  };
}

export function startTestServer(
  options: TestServerOptions = {},
): Promise<TestServer> {
  return startRustReactServer(options);
}

/**
 * One read-only tool, staged so a failed pass triggers the host's chat-only
 * retry. It is never called — what matters is that the pass was offered tools.
 */
export const echoContributor: ToolSetContributor = {
  namespace: "fixture",
  contribute: async () => [
    {
      definition: {
        name: "echo",
        version: "1.0.0",
        effect: "read",
        capability: "echo",
        description: "Echo the input.",
        inputSchema: {
          type: "object",
          properties: { text: { type: "string" } },
        },
      },
      async execute(ctx, input: unknown) {
        return {
          ok: true,
          data: input,
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    },
  ],
};

/**
 * A provider that streams half an answer, dies, and answers properly when asked
 * again — the host's chat-only retry, which runs when a pass FAILED and tools
 * were staged. Two passes, one run, one log: `run.started`, a delta,
 * `run.failed`, then `run.started`, a delta, `run.completed`.
 */
export class RetryingProvider extends MockProviderClient {
  calls = 0;
  /**
   * Held between the first pass's delta and its failure, so the two passes
   * reach the browser as separate renders. Without it the whole log lands in
   * one SSE batch and React coalesces it into a single state update — the end
   * state would be right and the LIVE text, which is what this models, would
   * never have been rendered at all.
   */
  static readonly PASS_GAP_MS = 60;

  override async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.calls += 1;
    const pass = this.calls;
    const stamp = createTestEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: nowIso(),
      data: { delta: pass === 1 ? "PASS-ONE" : "PASS-TWO" },
    });
    await new Promise((resolve) =>
      setTimeout(resolve, RetryingProvider.PASS_GAP_MS),
    );
    if (pass === 1) {
      yield stamp({
        type: "run.failed",
        runId: input.runId,
        timestamp: nowIso(),
        data: {
          errorCode: "unsupported_tools",
          errorMessage: "this endpoint cannot take tools",
        },
      });
      return;
    }
    yield stamp({
      type: "run.message.completed",
      runId: input.runId,
      timestamp: nowIso(),
      data: { content: "PASS-TWO", toolCallCount: 0 },
    });
  }
}

export interface ScriptedStream {
  fetch: FetchLike;
  /** How many `GET /runs/:id/stream` requests were served. */
  readonly opened: () => number;
}

function settledStreamBody(
  body: string,
  settlement?: Promise<void>,
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let sent = false;
  let cancelled = false;
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      if (!sent) {
        sent = true;
        controller.enqueue(encoder.encode(body));
        return;
      }
      await settlement;
      if (cancelled) return;
      controller.enqueue(
        encoder.encode("event: agentkit.stream.settled\ndata: {}\n\n"),
      );
      controller.close();
    },
    cancel() {
      cancelled = true;
    },
  });
}

/**
 * A `fetch` that scripts the RUN STREAM and leaves every other route real.
 *
 * The two shapes it exists for cannot be produced by a scripted provider: a
 * task that goes terminal WITHOUT a terminal event on its log (the host's
 * `failQuietly` writes one only best-effort, and `sse.ts` closes on the task's
 * status precisely so that log can end), and a `run.completed` carrying a
 * `finishReason` the mock provider does not emit. Everything else — the chat,
 * the messages the reconcile reads — is the real server, because the point is
 * what the HOOK does with a real reconcile around a scripted stream.
 */
export function scriptedStreamFetch(options: {
  /** Events sent before the settlement control frame and clean EOF. */
  events: (runId: string) => AiRunEvent[];
  /** What `getRun` should report instead of the run's real status. */
  runStatus?: RunStatusDto;
  /** Hold the stream open until the scripted host is ready to settle. */
  settlement?: Promise<void>;
}): ScriptedStream {
  let opened = 0;
  const wrapped: FetchLike = async (url, init) => {
    const stream = /\/runs\/([^/?]+)\/stream/.exec(url);
    if (stream !== null) {
      opened += 1;
      const body = options
        .events(stream[1] ?? "")
        .map(
          (event) =>
            `id: ${event.eventId}\nevent: ${event.type}\n` +
            `data: ${JSON.stringify(event)}\n\n`,
        )
        .join("");
      return new Response(settledStreamBody(body, options.settlement), {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      });
    }
    if (options.runStatus !== undefined && /\/runs\/[^/?]+$/.test(url)) {
      const real = await fetch(url, init);
      if (!real.ok) return real;
      const dto = (await real.json()) as Record<string, unknown>;
      return Response.json({ ...dto, status: options.runStatus });
    }
    return fetch(url, init);
  };
  return { fetch: wrapped, opened: () => opened };
}

/** One synthetic run event, for {@link scriptedStreamFetch}. */
export function scriptedEvent(
  runId: string,
  seq: number,
  type: AiRunEvent["type"],
  data: Record<string, unknown>,
): AiRunEvent {
  return {
    type,
    runId,
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data,
  } as AiRunEvent;
}

/** A provider that streams `count` deltas — a run long enough to cut in half. */
export function chattyProvider(count: number): MockProviderClient {
  const provider = new MockProviderClient();
  provider.setScript([
    {
      steps: Array.from({ length: count }, (_unused, index) => ({
        kind: "text" as const,
        content: `chunk-${String(index).padStart(3, "0")} `,
      })),
    },
  ]);
  return provider;
}
