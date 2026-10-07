/**
 * A real HTTP server, in-process, for the client to talk to.
 *
 * The client's whole job is HTTP and SSE framing, so testing it against a
 * hand-written stub would test the stub: the resume path in particular only
 * means anything against a server that really replays from `Last-Event-ID`, over
 * a body that really arrives in chunks. So this wires the actual stack —
 * `@agentkit/transport-http` over the reference in-memory store, the real
 * single-process queue and `TurnRunner`, a scripted provider — and puts
 * `Bun.serve` on an ephemeral port in front of it.
 *
 * Mirrors `packages/transport-http/tests/support/fixture.ts`'s `createLiveFixture`
 * rather than importing it: that fixture is another package's test-only file, and
 * a cross-package relative import into `tests/` would tie this suite to a path
 * nothing publishes.
 */
import {
  MemoryAssistantStore,
  MemorySecretStore,
} from "@agentkit/adapters-memory";
import {
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import {
  createTestEventStamper,
  MockProviderClient,
  nowIso,
} from "@agentkit/testing";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  createRestHandler,
  type RestHandlerDeps,
} from "@agentkit/transport-http";

export const TEST_CHAT_ID = "chat-client";
export const SEEDED_API_KEY = "sk-never-published-abcdef";

export interface TestServer {
  baseUrl: string;
  store: MemoryAssistantStore;
  provider: MockProviderClient;
  secrets: MemorySecretStore;
  stop(): Promise<void>;
}

export interface TestServerOptions {
  /** The turn the mock provider plays. Defaults to a single short answer. */
  provider?: MockProviderClient;
  /** Merged over the wiring — `authenticate` for the auth-header test. */
  deps?: Partial<RestHandlerDeps>;
}

export async function startTestServer(
  options: TestServerOptions = {},
): Promise<TestServer> {
  const store = new MemoryAssistantStore();
  const secrets = new MemorySecretStore();
  const taskRunner = new SingleProcessTaskRunner({
    store,
    clock: defaultClock,
    pollMs: 5,
    heartbeatMs: 60_000,
    shutdownMode: "cancel",
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
    { concurrency: 1, ownerId: "owner-client" },
  );

  const handler = createRestHandler({
    store,
    turns,
    tasks,
    secrets,
    // Short enough that a test does not sit out a poll interval; the retry hint
    // is small so the resume test can assert the client honoured it without
    // waiting seconds for the reconnect.
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
    secrets,
    async stop() {
      try {
        await worker.stop();
      } finally {
        await server.stop(true);
      }
    },
  };
}

function defaultProvider(): MockProviderClient {
  const provider = new MockProviderClient();
  provider.setScript([
    { steps: [{ kind: "text", content: "Hello, client." }] },
  ]);
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

/**
 * A provider that streams one delta and then PARKS until the run is cancelled.
 *
 * A `MockProviderClient` subclass rather than `@agentkit/testing`'s
 * `HangingProviderClient` only because the fixture here is typed to the mock —
 * the behaviour is the same, and it is what makes "the run was demonstrably
 * still live" a fact of the test rather than a race against a fast provider.
 */
export class ParkingProvider extends MockProviderClient {
  private announce!: () => void;
  private announceStopped!: () => void;
  /** Resolves the first time the stream parks. */
  readonly parked: Promise<void> = new Promise<void>((resolve) => {
    this.announce = resolve;
  });
  readonly stopped: Promise<void> = new Promise<void>((resolve) => {
    this.announceStopped = resolve;
  });
  cancelled = false;

  override async *streamChat(
    input: Parameters<MockProviderClient["streamChat"]>[0],
  ): AsyncIterable<AiRunEvent> {
    const stamp = createTestEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: nowIso(),
      data: { delta: "one" },
    });
    try {
      this.announce();
      await new Promise<void>((resolve) => {
        const signal = input.signal;
        if (signal === undefined || signal.aborted) {
          resolve();
          return;
        }
        signal.addEventListener("abort", () => resolve(), { once: true });
      });
      this.cancelled = input.signal?.aborted === true;
      const aborted = new Error("The operation was aborted.");
      aborted.name = "AbortError";
      throw aborted;
    } finally {
      this.announceStopped();
    }
  }
}

/** The provider, settings and chat rows a host writes before any turn. */
async function seedHostState(store: MemoryAssistantStore): Promise<void> {
  await store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    apiKey: SEEDED_API_KEY,
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

/** Poll until `predicate` holds, or fail loudly about what never happened. */
export async function waitFor(
  predicate: () => Promise<boolean>,
  what: string,
  // Real-socket e2e cases have flaked under concurrent test load with the old
  // 5 s budget; a generous ceiling costs nothing when the predicate holds early.
  timeoutMs = 10_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  throw new Error(`Timed out waiting for ${what}.`);
}
