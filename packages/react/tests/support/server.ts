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
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import type { AiProviderClient } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
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
    contributors: [],
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
