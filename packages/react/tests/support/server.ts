/**
 * A real HTTP server, in-process, for the hooks to talk to.
 *
 * These hooks are a thin layer over `@agentkit/client`, and the interesting
 * half of what they do — optimistic record, streamed delta, terminal event,
 * reconcile — is only interesting against a server that really streams: a
 * mocked client handing back a pre-built array would test the array. So this
 * wires the actual stack (`@agentkit/transport-http` over the reference
 * in-memory store, the real single-process queue and `TurnRunner`, a scripted
 * provider) behind `Bun.serve` on an ephemeral port.
 *
 * Mirrors `packages/client/tests/support/server.ts` rather than importing it,
 * for the reason that file gives for mirroring transport-http's: another
 * package's test-only file is not a module path anything publishes.
 */
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import {
  ChatTurnExecutor,
  ExecutorRegistry,
  ProposalService,
  SessionWritePolicy,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
  type ApplyOutcome,
  type ApplyProposalInput,
  type ProposalApplier,
  type ToolSetContributor,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  createTestEventStamper,
  MockProviderClient,
  nowIso,
} from "@agentkit/testing";
import {
  createRestHandler,
  type RestHandlerDeps,
} from "@agentkit/transport-http";

export const TEST_CHAT_ID = "chat-react";

export interface TestServer {
  baseUrl: string;
  store: MemoryAssistantStore;
  provider: AiProviderClient;
  /** The staged-write pipeline the decision routes need. */
  proposals: ProposalService;
  /** Where a write-only `apiKey` lands — proof it went there and nowhere else. */
  secrets: MemorySecretStore;
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
  deps?: Partial<RestHandlerDeps>;
}

export async function startTestServer(
  options: TestServerOptions = {},
): Promise<TestServer> {
  const store = new MemoryAssistantStore();
  const taskRunner = new SingleProcessTaskRunner({
    store,
    clock: defaultClock,
    pollMs: 5,
    heartbeatMs: 60_000,
  });
  const provider = options.provider ?? defaultProvider();
  const turns = new TurnRunner({
    store,
    taskRunner,
    providerFactory: () => provider,
    contributors: options.contributors ?? [],
    clock: defaultClock,
    ids: defaultIds,
  });
  const tasks = new TaskService({
    store,
    taskRunner,
    ids: defaultIds,
    clock: defaultClock,
  });

  await seedHostState(store);

  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(turns));
  const worker = await taskRunner.startWorker(
    createDispatchingWorker(registry, { store, clock: defaultClock }),
    { concurrency: 1, ownerId: "owner-react" },
  );

  const secrets = new MemorySecretStore();
  const proposals = new ProposalService({
    store,
    applier: new RecordingApplier(),
    policy: new SessionWritePolicy({ clock: defaultClock }),
    clock: defaultClock,
    ids: defaultIds,
  });

  const handler = createRestHandler({
    store,
    turns,
    tasks,
    secrets,
    proposals,
    streaming: {
      pollIntervalMs: 5,
      heartbeatIntervalMs: 60_000,
      retryHintMs: 10,
    },
    ...options.deps,
  });

  const server = Bun.serve({ port: 0, fetch: handler, idleTimeout: 0 });

  return {
    baseUrl: `http://127.0.0.1:${server.port}`,
    store,
    provider,
    proposals,
    secrets,
    async stop() {
      await worker.stop();
      await server.stop(true);
    },
  };
}

/**
 * An applier that succeeds and remembers, keyed by `operationId`.
 *
 * Enough to prove the apply route ran once per operation id — what the write
 * actually DOES is the host application's business, and a test applier that
 * modelled one would be testing itself.
 */
class RecordingApplier implements ProposalApplier {
  readonly outcomes = new Map<string, ApplyOutcome>();

  async apply(input: ApplyProposalInput): Promise<ApplyOutcome> {
    const outcome: ApplyOutcome = {
      status: "applied",
      appliedOps: input.proposal.operations.length,
      failedOps: [],
    };
    this.outcomes.set(input.operationId, outcome);
    return outcome;
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    return this.outcomes.get(operationId) ?? null;
  }
}

/** A `SecretStore` in a Map — enough to keep a written key out of every read. */
export class MemorySecretStore {
  readonly values = new Map<string, string>();

  async get(ref: string): Promise<string | null> {
    return this.values.get(ref) ?? null;
  }

  async set(ref: string, value: string): Promise<void> {
    this.values.set(ref, value);
  }

  async delete(ref: string): Promise<void> {
    this.values.delete(ref);
  }

  async listRefs(): Promise<string[]> {
    return [...this.values.keys()];
  }
}

function defaultProvider(): MockProviderClient {
  const provider = new MockProviderClient();
  provider.setScript([{ steps: [{ kind: "text", content: "Hello, hooks." }] }]);
  return provider;
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
    if (pass === 1) throw new Error("this endpoint cannot take tools");
    yield stamp({
      type: "run.message.completed",
      runId: input.runId,
      timestamp: nowIso(),
      data: { content: "PASS-TWO", toolCallCount: 0 },
    });
  }
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

/** The provider, settings and chat rows a host writes before any turn. */
async function seedHostState(store: MemoryAssistantStore): Promise<void> {
  await store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    apiKey: "sk-never-published-abcdef",
    defaultModel: "m1",
    enabled: true,
  });
  await store.providers.replaceModels("p1", [
    {
      providerId: "p1",
      modelId: "m1",
      displayName: "Mock model",
      fetchedAt: new Date(0).toISOString(),
    },
  ]);
  await store.settings.updateSettings({ defaultProviderId: "p1" });
  await store.conversations.createChat({ id: TEST_CHAT_ID });
}
