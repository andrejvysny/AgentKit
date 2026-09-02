/**
 * The wiring every test in this package calls the handler through.
 *
 * Two shapes, because the two questions are different. {@link createHandlerFixture}
 * wires the real `TurnRunner` and `TaskService` over the reference in-memory
 * store but hands them a queue that never runs anything — so a submitted run
 * stays `queued`, which is exactly the state the routing, idempotency and
 * cancel assertions want to observe. {@link createLiveFixture} adds the real
 * single-process queue and a scripted provider, for the tests that need a turn
 * to actually happen.
 */
import {
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
  type EnqueueInput,
  type StartWorkerOptions,
  type TaskRunner,
  type TaskWorker,
  type WorkerHandle,
} from "@agentkit/host";
import {
  MemoryAssistantStore,
  MemorySecretStore,
} from "@agentkit/adapters-memory";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import { MockProviderClient } from "@agentkit/testing";
import {
  createRestHandler,
  type RestFetchHandler,
  type RestHandlerDeps,
} from "../../src/index.js";

/** Re-exported so tests can `import { MemorySecretStore } from "./support/fixture.js"`. */
export { MemorySecretStore };

export const TEST_CHAT_ID = "chat-http";

/** A queue that records pokes and runs nothing: every task stays `queued`. */
export class InertTaskRunner implements TaskRunner {
  readonly enqueued: string[] = [];
  readonly cancelRequests: string[] = [];

  async enqueue(input: EnqueueInput): Promise<void> {
    this.enqueued.push(input.taskId);
  }

  async requestCancel(taskId: string): Promise<void> {
    this.cancelRequests.push(taskId);
  }

  async recover(): Promise<void> {}

  async startWorker(
    _worker: TaskWorker,
    _opts?: StartWorkerOptions,
  ): Promise<WorkerHandle> {
    return { stop: async () => {} };
  }
}

export interface HandlerFixture {
  store: MemoryAssistantStore;
  runner: InertTaskRunner;
  turnRunner: TurnRunner;
  handler: RestFetchHandler;
}

export async function createHandlerFixture(
  overrides: Partial<RestHandlerDeps> = {},
): Promise<HandlerFixture> {
  const store = new MemoryAssistantStore();
  const runner = new InertTaskRunner();
  const provider = new MockProviderClient();
  provider.setScript([{ steps: [{ kind: "text", content: "Hi there." }] }]);

  const turnRunner = new TurnRunner({
    store,
    taskRunner: runner,
    providerFactory: () => provider,
    contributors: [],
    clock: defaultClock,
    ids: defaultIds,
  });
  const tasks = new TaskService({
    store,
    taskRunner: runner,
    ids: defaultIds,
    clock: defaultClock,
  });

  await seedHostState(store);

  const handler = createRestHandler({
    store,
    turns: turnRunner,
    tasks,
    ...overrides,
  });
  return { store, runner, turnRunner, handler };
}

export interface LiveFixture {
  store: MemoryAssistantStore;
  provider: MockProviderClient;
  /** The wiring itself, so an e2e test can hand it to `serveRest`. */
  deps: RestHandlerDeps;
  handler: RestFetchHandler;
  stop(): Promise<void>;
}

/** The full stack: real queue, real turn worker, scripted provider. */
export async function createLiveFixture(
  script?: MockProviderClient,
): Promise<LiveFixture> {
  const store = new MemoryAssistantStore();
  const taskRunner = new SingleProcessTaskRunner({
    store,
    clock: defaultClock,
    pollMs: 5,
    heartbeatMs: 60_000,
  });
  const provider = script ?? defaultProvider();
  const turnRunner = new TurnRunner({
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
  registry.register(new ChatTurnExecutor(turnRunner));
  const handle = await taskRunner.startWorker(
    createDispatchingWorker(registry, { store, clock: defaultClock }),
    { concurrency: 1, ownerId: "owner-http" },
  );

  const deps: RestHandlerDeps = {
    store,
    turns: turnRunner,
    tasks,
    // Short enough that a test does not sit on the poll interval, long enough
    // that it is not the thing being measured.
    streaming: { pollIntervalMs: 5, heartbeatIntervalMs: 60_000 },
  };
  return {
    store,
    provider,
    deps,
    handler: createRestHandler(deps),
    stop: () => handle.stop(),
  };
}

function defaultProvider(): MockProviderClient {
  const provider = new MockProviderClient();
  provider.setScript([
    { steps: [{ kind: "text", content: "Hello from the mock." }] },
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
    apiKey: "sk-should-never-be-published",
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
  timeoutMs = 5_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  throw new Error(`Timed out waiting for ${what}.`);
}

/** `new Request` against a stable origin — the handler only reads the path. */
export function request(
  method: string,
  path: string,
  init: { body?: unknown; headers?: Record<string, string> } = {},
): Request {
  return new Request(`http://rest.test${path}`, {
    method,
    ...(init.body === undefined ? {} : { body: JSON.stringify(init.body) }),
    headers: {
      ...(init.body === undefined
        ? {}
        : { "content-type": "application/json" }),
      ...(init.headers ?? {}),
    },
  });
}
