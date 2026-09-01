/**
 * `useRun`, including the two properties that only exist against a real socket:
 * a stream that survives being severed, and one that stops the moment the
 * component goes away.
 *
 * THE SEVERING FETCH is the trick `packages/client/tests/resume.test.ts` uses,
 * reproduced here for the same reason: a reset connection is bytes followed by
 * a REJECTED read, which is not something a mock `fetch` returning a finished
 * body can express. The cut is counted in SSE frames rather than bytes so that
 * "one connection carried five frames" is a fact about the test rather than
 * about the day's chunking.
 */
import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import {
  createAgentKitClient,
  type AgentKitClient,
  type FetchLike,
} from "@agentkit/client";
import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import type { TaskEventEnvelope } from "@agentkit/contracts";
import { HangingProviderClient } from "@agentkit/testing";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useRun } from "../src/index.js";
import { strictWrapper, wrapper } from "./support/render.js";
import {
  chattyProvider,
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;

beforeEach(async () => {
  server = await startTestServer({ provider: chattyProvider(40) });
});

afterEach(async () => {
  await server.stop();
});

interface Severing {
  fetch: FetchLike;
  readonly resumeHeaders: (string | null)[];
  readonly cuts: () => number;
}

/**
 * A `fetch` that severs the first `cutsWanted` stream bodies after exactly
 * `framesPerConnection` SSE frames.
 *
 * The error is raised on the pull AFTER the budget is spent, never in the same
 * one: `controller.error()` discards whatever is still queued, so erroring
 * immediately after an `enqueue` would throw away the chunk carrying the last
 * event the client saw — and the resume would then start from an id it was
 * never handed.
 */
function severingFetch(framesPerConnection: number, cutsWanted = 1): Severing {
  const resumeHeaders: (string | null)[] = [];
  let cuts = 0;

  const wrapped: FetchLike = async (url, init) => {
    const isStream = url.includes("/stream");
    if (isStream) {
      resumeHeaders.push(new Headers(init?.headers ?? {}).get("last-event-id"));
    }
    const response = await fetch(url, init);
    if (!isStream || cuts >= cutsWanted || response.body === null) {
      return response;
    }
    cuts += 1;

    const upstream = response.body.getReader();
    let forwarded = 0;
    let spent = framesPerConnection === 0;
    let carry = new Uint8Array(0);

    const body = new ReadableStream<Uint8Array>({
      async pull(controller) {
        if (spent) {
          await upstream.cancel().catch(() => undefined);
          controller.error(new Error("connection reset by the test proxy"));
          return;
        }
        const chunk = await upstream.read();
        if (chunk.done) {
          if (carry.length > 0) controller.enqueue(carry);
          controller.close();
          return;
        }
        const buffer = concat(carry, chunk.value);
        carry = new Uint8Array(0);

        let cut = -1;
        for (let i = 0; i + 1 < buffer.length; i += 1) {
          if (buffer[i] !== 0x0a || buffer[i + 1] !== 0x0a) continue;
          forwarded += 1;
          if (forwarded >= framesPerConnection) {
            cut = i + 2;
            break;
          }
          i += 1;
        }
        if (cut === -1) {
          controller.enqueue(buffer);
          return;
        }
        spent = true;
        controller.enqueue(buffer.slice(0, cut));
      },
      cancel() {
        void upstream.cancel().catch(() => undefined);
      },
    });
    return new Response(body, {
      status: response.status,
      headers: response.headers,
    });
  };

  return { fetch: wrapped, resumeHeaders, cuts: () => cuts };
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.length === 0) return b;
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

async function startRun(client: AgentKitClient): Promise<string> {
  const submitted = await client.submitMessage(
    { chatId: TEST_CHAT_ID },
    { content: "stream me" },
  );
  return submitted.result.runId;
}

/** The run's whole durable log, straight out of the store. */
async function logOf(runId: string): Promise<AiRunEvent[]> {
  const events = await server.store.tasks.listEvents(runId);
  return [...events].sort((a, b) => a.seq - b.seq) as AiRunEvent[];
}

describe("useRun", () => {
  test("streams a run to its terminal event", async () => {
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const runId = await startRun(client);

    const { result } = renderHook(() => useRun(runId), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.phase).toBe("completed"), {
      timeout: 10_000,
    });

    const log = await logOf(runId);
    expect(result.current.events.map((e) => e.eventId)).toEqual(
      log.map((e) => e.eventId),
    );
    expect(result.current.error).toBeNull();
  });

  test("resumes across a severed connection, every event exactly once", async () => {
    const severing = severingFetch(5);
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: severing.fetch,
    });
    const runId = await startRun(client);

    const { result } = renderHook(() => useRun(runId), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.phase).toBe("completed"), {
      timeout: 10_000,
    });

    expect(severing.cuts()).toBe(1);
    // Two connections: the first was severed, the second carried a resume.
    expect(severing.resumeHeaders).toHaveLength(2);
    expect(severing.resumeHeaders[0]).toBeNull();
    expect(severing.resumeHeaders[1]).toBeString();

    const log = await logOf(runId);
    const seen = result.current.events;
    expect(seen.map((e) => e.eventId)).toEqual(log.map((e) => e.eventId));
    expect(seen.map((e) => e.seq)).toEqual(seen.map((_e, i) => i));
    expect(new Set(seen.map((e) => e.eventId)).size).toBe(seen.length);
  });

  test("unmounting mid-stream aborts and leaves the state where it was", async () => {
    await server.stop();
    // Parked mid-turn, so the run is DEMONSTRABLY unfinished at the unmount —
    // a fast provider would finish first and the assertion would prove nothing.
    const hanging = new HangingProviderClient({ deltas: ["a", "b", "c"] });
    server = await startTestServer({ provider: hanging });
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const runId = await startRun(client);

    const { result, unmount } = renderHook(() => useRun(runId), {
      wrapper: wrapper(client),
    });
    await hanging.whenBlocking();
    await waitFor(() => expect(result.current.phase).toBe("streaming"));
    const atUnmount = result.current.events.length;
    unmount();

    // The run reaches its terminal event AFTER the unmount, over a stream
    // nothing is reading any more.
    await client.cancelRun({ runId });
    await waitFor(async () => {
      expect((await client.getRun({ runId })).status).toBe("cancelled");
    });
    expect(result.current.events.length).toBe(atUnmount);
    expect(result.current.phase).toBe("streaming");
    expect(result.current.error).toBeNull();
  });

  test("drain picks up an event appended after the terminal one", async () => {
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const runId = await startRun(client);

    const { result } = renderHook(() => useRun(runId), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.phase).toBe("completed"), {
      timeout: 10_000,
    });
    const live = result.current.events.length;
    expect(
      result.current.events.some((e) => e.type === "run.verification"),
    ).toBe(false);

    const trailing = await appendTrailing(runId);
    await act(async () => {
      await result.current.drain();
    });

    expect(result.current.events).toHaveLength(live + 1);
    expect(result.current.events.at(-1)?.eventId).toBe(trailing.eventId);
    expect(result.current.events.at(-1)?.type).toBe("run.verification");
  });

  test("under <StrictMode> the doubled effect yields no duplicate events", async () => {
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const runId = await startRun(client);

    const { result } = renderHook(() => useRun(runId), {
      wrapper: strictWrapper(client),
    });
    await waitFor(() => expect(result.current.phase).toBe("completed"), {
      timeout: 10_000,
    });

    const ids = result.current.events.map((e) => e.eventId);
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids).toEqual((await logOf(runId)).map((e) => e.eventId));
  });

  test("a run that does not exist is a typed 404 in state, not a throw", async () => {
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const { result } = renderHook(() => useRun("run-nope"), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.error).not.toBeNull());
    expect(result.current.error).toMatchObject({
      status: 404,
      code: "not_found",
    });
  });

  test("useRun(null) is inert", () => {
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const { result } = renderHook(() => useRun(null), {
      wrapper: wrapper(client),
    });
    expect(result.current.events).toEqual([]);
    expect(result.current.phase).toBeNull();
  });
});

/** Append one event to a finished run's log, the way a late pass would. */
async function appendTrailing(runId: string): Promise<AiRunEvent> {
  const seq = await server.store.tasks.nextSeq(runId);
  const full = {
    type: "run.verification",
    runId,
    timestamp: new Date().toISOString(),
    data: { pass: 1, status: "partial", deficiencies: ["one thing"] },
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-trailing-${seq}`,
    seq,
  } as AiRunEvent;
  const lease = await server.store.tasks.acquireLease({
    taskId: runId,
    attemptId: `att-trailing-${seq}`,
    ownerId: "trailing-writer",
    ttlMs: 60_000,
  });
  await server.store.tasks.appendEvents(runId, [full as TaskEventEnvelope], {
    leaseToken: lease.leaseToken,
  });
  return full;
}
