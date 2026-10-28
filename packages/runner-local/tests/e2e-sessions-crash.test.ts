/**
 * The M2 acceptance gate: a crash/restart matrix for durable agent sessions
 * (ADR 0016; crash matrix C1-C8 in docs/baseline/pi-parity-scenarios.md), run on
 * BOTH the memory and the sqlite adapter.
 *
 * "Restart" here means a NEW runner + `TurnRunner` + `AgentSessionService` over
 * the same store (for sqlite: a new handle on the same FILE). The old runner is
 * abandoned: its provider call / tool call is parked on a promise the abort
 * signal cannot reach (a process that stopped existing), the fake clock moves
 * past the lease TTL, and the new runner's `recover()` hands the task attempt 2.
 * The old attempt is the ZOMBIE: released at the end, it must land nothing.
 *
 * Every case ends in {@link assertEndState}: gapless event seq with unique
 * ids, no duplicate tool/assistant records, the expected task status and
 * attempt count, and (where a zombie exists) a fence that refuses it.
 *
 * These tests model owner loss in process. The host's approval-sigkill tests
 * separately exercise abrupt OS process death and persistent SQLite effects.
 */
import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import { humanRequestId } from "@agentkit/core";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import {
  AgentSessionService,
  ChatTurnExecutor,
  ExecutorRegistry,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  createUserInputContributor,
  type AssistantStore,
  type IdGenerator,
  type MessageRecord,
  type TaskExecution,
  type TaskStatus,
  type TaskWorker,
  type ToolSetContributor,
  type WorkerHandle,
} from "@agentkit/host";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import { SingleProcessTaskRunner } from "../src/index.js";
import {
  createTestClock,
  waitFor,
  type TestClock,
} from "./support/task-runner-harness.js";

const SESSION = "s1";
/** Both adapters' default lease TTL. */
const LEASE_TTL_MS = 30_000;
const ZOMBIE_ANSWER = "ZOMBIE-ANSWER";
const ANSWER_2 = "ANSWER-FROM-ATTEMPT-2";

type Adapter = "memory" | "sqlite";

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/** A place to die and to wake up in much later; not reachable by any signal. */
class Gate {
  private announce!: () => void;
  private open!: () => void;
  private readonly parked = new Promise<void>((resolve) => {
    this.announce = resolve;
  });
  private readonly opened = new Promise<void>((resolve) => {
    this.open = resolve;
  });
  whenParked(): Promise<void> {
    return this.parked;
  }
  async park(): Promise<void> {
    this.announce();
    await this.opened;
  }
  release(): void {
    this.open();
  }
}

interface CallPlan {
  turn: MockTurn;
  /** Die before emitting anything, or after the deltas but before the answer. */
  park?: "start" | "beforeCompleted";
  /** Runs at the top of the call, while the turn is demonstrably in flight. */
  before?: () => Promise<void>;
}

/** One scripted model call per plan entry; records what each call was handed. */
class PlannedProvider implements AiProviderClient {
  readonly id = "planned";
  readonly kind = "openai-compatible" as const;
  readonly calls: AiChatMessage[][] = [];
  /** Parked calls that were released and carried on (the zombie woke up). */
  resumed = 0;
  private readonly gates = new Map<number, Gate>();

  constructor(private readonly plan: CallPlan[]) {}

  gate(callNumber: number): Gate {
    let gate = this.gates.get(callNumber);
    if (!gate) {
      gate = new Gate();
      this.gates.set(callNumber, gate);
    }
    return gate;
  }

  releaseAll(): void {
    for (const gate of this.gates.values()) gate.release();
  }

  async capabilities() {
    return { streaming: true, toolCalling: true, modelList: true };
  }

  async listModels() {
    return [];
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const n = this.calls.push(structuredClone(input.messages));
    const entry: CallPlan = this.plan[n - 1] ?? {
      turn: { steps: [{ kind: "text", content: "UNPLANNED" }] },
    };
    await entry.before?.();
    if (entry.park === "start") {
      await this.gate(n).park();
      this.resumed += 1;
    }
    const inner = new MockProviderClient();
    inner.setScript([entry.turn]);
    for await (const event of inner.streamChat(input)) {
      if (
        entry.park === "beforeCompleted" &&
        event.type === "run.message.completed"
      ) {
        await this.gate(n).park();
        this.resumed += 1;
      }
      yield event;
    }
  }
}

const text = (content: string): MockTurn => ({
  steps: [{ kind: "text", content }],
});
const toolCall = (toolCallId: string, name: string, args = "{}"): MockTurn => ({
  steps: [{ kind: "tool_call", toolCallId, name, argumentsJson: args }],
});

// ---------------------------------------------------------------------------
// World: one store (or one sqlite file), one clock, many runner generations
// ---------------------------------------------------------------------------

interface Node {
  store: AssistantStore;
  taskRunner: SingleProcessTaskRunner;
  turnRunner: TurnRunner;
  service: AgentSessionService;
  executions: TaskExecution[];
  worker: WorkerHandle | null;
  startWorker(): Promise<void>;
  stopWorker(): Promise<void>;
}

interface World {
  adapter: Adapter;
  clock: TestClock;
  ids: IdGenerator;
  provider: PlannedProvider;
  store: AssistantStore;
  /** Every sqlite handle ever opened, so a zombie's handle stays usable. */
  handles: Array<{ close(): void }>;
  dir: string | undefined;
  nodes: Node[];
  /** Side-effect counter of the `bump` tool. */
  bumps: number;
  blockFirstBump: boolean;
  toolGate: Gate;
  onEcho: (() => Promise<void>) | undefined;
}

function counterIds(): IdGenerator {
  const counters = new Map<string, number>();
  const next = (kind: string): string => {
    const n = (counters.get(kind) ?? 0) + 1;
    counters.set(kind, n);
    return `${kind}-${n}`;
  };
  return {
    taskId: () => next("task"),
    attemptId: () => next("att"),
    eventId: () => next("evt"),
    proposalId: () => next("prp"),
    operationId: () => next("op"),
    messageId: () => next("msg"),
    chatId: () => next("chat"),
  };
}

let worlds: World[] = [];

afterEach(async () => {
  for (const world of worlds) {
    world.provider.releaseAll();
    world.toolGate.release();
    for (const node of world.nodes) await node.worker?.stop();
    for (const handle of world.handles) {
      try {
        handle.close();
      } catch {
        // already closed by a clean quit
      }
    }
    if (world.dir) rmSync(world.dir, { recursive: true, force: true });
  }
  worlds = [];
});

function openStore(world: World): AssistantStore {
  const options = { clock: world.clock, ids: world.ids };
  if (world.adapter === "memory") {
    const store = new MemoryAssistantStore(options);
    world.handles.push({ close() {} });
    return store;
  }
  const store = new SqliteAssistantStore(join(world.dir!, "store.db"), options);
  world.handles.push(store);
  return store;
}

async function createWorld(adapter: Adapter, plan: CallPlan[]): Promise<World> {
  const world: World = {
    adapter,
    clock: createTestClock(),
    ids: counterIds(),
    provider: new PlannedProvider(plan),
    store: undefined as unknown as AssistantStore,
    handles: [],
    dir:
      adapter === "sqlite"
        ? mkdtempSync(join(tmpdir(), "agentkit-sessions-crash-"))
        : undefined,
    nodes: [],
    bumps: 0,
    blockFirstBump: false,
    toolGate: new Gate(),
    onEcho: undefined,
  };
  worlds.push(world);
  world.store = openStore(world);
  await world.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await world.store.settings.updateSettings({ defaultProviderId: "p1" });
  return world;
}

function toolContributor(world: World): ToolSetContributor {
  const result = (limits: never, summary: string) => ({
    ok: true as const,
    data: { n: world.bumps },
    sources: [],
    warnings: [],
    truncated: false,
    limits,
    summary,
  });
  const tool = (
    name: string,
    execute: (ctx: { limits: never }) => Promise<ReturnType<typeof result>>,
    effect: "read" | "write" = "read",
  ): AiTool =>
    ({
      definition: {
        name,
        version: "1.0.0",
        effect,
        capability: name,
        description: name,
        inputSchema: { type: "object", properties: {} },
      },
      execute: (ctx: { limits: never }) => execute(ctx),
    }) as unknown as AiTool;
  // `bump` declares itself a write: it mutates something outside the runtime,
  // which is exactly what side-effect reconciliation is for.
  const bump = tool(
    "bump",
    async (ctx) => {
      world.bumps += 1;
      // The "process dies" half: the side effect happened, nothing was logged.
      if (world.blockFirstBump && world.bumps === 1)
        await world.toolGate.park();
      return result(ctx.limits, "bumped");
    },
    "write",
  );
  const echo = tool("echo_steer", async (ctx) => {
    await world.onEcho?.();
    return result(ctx.limits, "echoed");
  });
  return { namespace: "test", contribute: async () => [bump, echo] };
}

interface NodeOptions {
  ownerId: string;
  /** Wrap the store the turn runner and the service see. */
  wrap?: (store: AssistantStore) => AssistantStore;
  /** Default true. */
  startWorker?: boolean;
}

async function startNode(world: World, options: NodeOptions): Promise<Node> {
  const raw = world.store;
  const store = options.wrap ? options.wrap(raw) : raw;
  const taskRunner = new SingleProcessTaskRunner({
    store: raw,
    clock: world.clock,
    pollMs: 5,
    // Far past any test's real lifetime: renewal must not rescue a lease.
    heartbeatMs: 60_000,
  });
  const turnRunner = new TurnRunner({
    store,
    taskRunner,
    providerFactory: () => world.provider,
    contributors: [
      createUserInputContributor({ clock: world.clock }),
      toolContributor(world),
    ],
    clock: world.clock,
    ids: world.ids,
  });
  const tasks = new TaskService({
    store,
    taskRunner,
    ids: world.ids,
    clock: world.clock,
  });
  const service = new AgentSessionService({
    store,
    turns: turnRunner,
    tasks,
    taskRunner,
    clock: world.clock,
    ids: world.ids,
  });
  const executions: TaskExecution[] = [];
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(turnRunner));
  const dispatching = createDispatchingWorker(registry, {
    store: raw,
    clock: world.clock,
  });
  const recording: TaskWorker = {
    execute(execution) {
      executions.push(execution);
      return dispatching.execute(execution);
    },
  };
  const node: Node = {
    store,
    taskRunner,
    turnRunner,
    service,
    executions,
    worker: null,
    async startWorker() {
      node.worker = await taskRunner.startWorker(recording, {
        concurrency: 1,
        ownerId: options.ownerId,
      });
    },
    async stopWorker() {
      await node.worker?.stop();
      node.worker = null;
    },
  };
  world.nodes.push(node);
  if (options.startWorker !== false) await node.startWorker();
  return node;
}

/** A NEW process: a fresh store handle on sqlite, the same Maps on memory. */
function reopenStore(world: World): void {
  if (world.adapter === "sqlite") world.store = openStore(world);
}

/** The old process is gone: lease expires, new runner recovers attempt 2. */
async function crashAndRecover(world: World, ownerId: string): Promise<Node> {
  world.clock.advance(LEASE_TTL_MS + 1);
  reopenStore(world);
  const node = await startNode(world, { ownerId });
  await node.taskRunner.recover();
  return node;
}

/** A clean quit: workers stopped and the old store handle closed. */
async function quit(world: World, node: Node): Promise<void> {
  await node.stopWorker();
  const old = world.handles[world.handles.length - 1];
  if (world.adapter === "sqlite") old?.close();
  reopenStore(world);
}

async function seedSession(node: Node): Promise<void> {
  await node.service.createSession({
    sessionId: SESSION,
    profile: { id: "default" },
    providerId: "p1",
    model: "m-x",
  });
}

async function sendTurn(node: Node, content: string, inputId?: string) {
  const result = await node.service.send(SESSION, {
    content,
    ...(inputId === undefined ? {} : { inputId }),
  });
  if (result.kind !== "turn") throw new Error("expected a turn");
  return result;
}

async function waitTask(
  world: World,
  taskId: string,
  status: string,
  attempts?: number,
): Promise<void> {
  await waitFor(
    async () => {
      const task = await world.store.tasks.getTask(taskId);
      return (
        task?.status === status &&
        (attempts === undefined || task.attemptCount === attempts)
      );
    },
    `task ${taskId} to be ${status}${attempts === undefined ? "" : ` (attempt ${attempts})`}`,
    5_000,
  );
}

const eventsOf = async (world: World, taskId: string) =>
  (await world.store.tasks.listEvents(taskId)) as AiRunEvent[];

const messagesOf = (world: World) =>
  world.store.conversations.listMessages(SESSION);

async function codeOf(promise: Promise<unknown>): Promise<string | undefined> {
  try {
    await promise;
  } catch (error) {
    return (error as { code?: string }).code;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// The shared end-of-case assertions
// ---------------------------------------------------------------------------

interface EndState {
  /** runId -> expected task status and attempt count. */
  runs: Record<string, { status: TaskStatus; attempts: number }>;
  zombie?: {
    runId: string;
    executions: TaskExecution[];
    eventsBefore: number;
    messagesBefore: number;
  };
}

async function assertEndState(world: World, end: EndState): Promise<void> {
  const store = world.store;
  const messages = await messagesOf(world);
  for (const [runId, expected] of Object.entries(end.runs)) {
    // (c) status and attempts.
    const task = await store.tasks.getTask(runId);
    expect(task?.status).toBe(expected.status);
    expect(task?.attemptCount).toBe(expected.attempts);

    // (a) one unbroken stream.
    const events = await eventsOf(world, runId);
    expect(events.map((event) => event.seq)).toEqual(
      events.map((_, index) => index),
    );
    expect(new Set(events.map((event) => event.eventId)).size).toBe(
      events.length,
    );

    // (b) no repeated tool call id within the run.
    const toolIds = messages
      .filter((m) => m.runId === runId && m.role === "tool")
      .map((m) => m.toolCallId);
    expect(new Set(toolIds).size).toBe(toolIds.length);
  }
  // (b) no two assistant records for the same provider step, and exactly one
  // visible answer record (the placeholder) per turn.
  const internal = messages.filter(
    (m) => m.role === "assistant" && m.metadata["internal"] === true,
  );
  const callIds = internal.flatMap((m) => (m.toolCalls ?? []).map((c) => c.id));
  expect(new Set(callIds).size).toBe(callIds.length);
  const visible = messages.filter(
    (m) => m.role === "assistant" && m.metadata["internal"] !== true,
  );
  expect(visible).toHaveLength(Object.keys(end.runs).length);

  // (d) the zombie is fenced: nothing landed while it woke, and its old lease
  // token is refused by both an event append and a terminal write.
  const { zombie } = end;
  if (zombie) {
    expect(await eventsOf(world, zombie.runId)).toHaveLength(
      zombie.eventsBefore,
    );
    expect(messages).toHaveLength(zombie.messagesBefore);
    expect(zombie.executions.length).toBeGreaterThan(0);
    const stale = zombie.executions[0]!;
    expect(
      await codeOf(
        store.tasks.appendEvents(zombie.runId, [], {
          leaseToken: stale.leaseToken,
        }),
      ),
    ).toBe("lease_lost");
    const before = await store.tasks.getTask(zombie.runId);
    await expect(
      store.tasks.transitionTask(
        zombie.runId,
        ["running", "waiting_approval", "queued"],
        "completed",
        { finishedAt: world.clock.nowIso() },
        { leaseToken: stale.leaseToken },
      ),
    ).rejects.toThrow();
    expect((await store.tasks.getTask(zombie.runId))?.status).toBe(
      before?.status,
    );
  }
}

/** Release the dead attempt's parks and let its execution finish. */
async function wakeZombie(world: World, zombieNode: Node): Promise<void> {
  world.provider.releaseAll();
  world.toolGate.release();
  await zombieNode.stopWorker();
}

const placeholderOf = (messages: MessageRecord[], id: string) =>
  messages.find((m) => m.id === id);

function countUser(request: AiChatMessage[], content: string): number {
  return request.filter((m) => m.role === "user" && m.content === content)
    .length;
}

// ---------------------------------------------------------------------------
// Fault injection: a store wrapper, not a product change
// ---------------------------------------------------------------------------

function overlay<T extends object>(
  target: T,
  patch: Record<string, unknown>,
): T {
  return new Proxy(target, {
    get(t, key) {
      if (typeof key === "string" && key in patch) return patch[key];
      const value = Reflect.get(t, key) as unknown;
      return typeof value === "function"
        ? (value as (...a: unknown[]) => unknown).bind(t)
        : value;
    },
  });
}

/** Run `fn` inside a transaction whose `tx` is decorated. */
function withTx(
  store: AssistantStore,
  decorate: (tx: AssistantStore) => AssistantStore,
  around?: (
    run: <T>(fn: (tx: AssistantStore) => Promise<T>) => Promise<T>,
  ) => <T>(fn: (tx: AssistantStore) => Promise<T>) => Promise<T>,
): AssistantStore {
  const run = <T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> =>
    store.transaction((tx) => fn(decorate(tx)));
  const transaction = around ? around(run) : run;
  return overlay(store, { transaction });
}

/**
 * The park transaction dies ONCE, at its first write, and the process with it:
 * the attempt hangs (lease still held, `run.waiting` already logged). Woken
 * later it retries the real transaction with its stale lease.
 */
function crashOnPark(store: AssistantStore): {
  store: AssistantStore;
  crashed: Gate;
} {
  const crashed = new Gate();
  let armed = true;
  const CRASH = new Error("simulated process death at park");
  const wrapped = withTx(
    store,
    (tx) =>
      overlay(tx, {
        tasks: overlay(tx.tasks, {
          transitionTask: (
            id: string,
            from: string[],
            to: string,
            ...rest: unknown[]
          ) => {
            if (armed && to === "waiting_approval") {
              armed = false;
              throw CRASH;
            }
            return (
              tx.tasks.transitionTask as (...a: unknown[]) => Promise<unknown>
            ).call(tx.tasks, id, from, to, ...rest);
          },
        }),
      }),
    (run) =>
      (async <T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> => {
        try {
          return await run(fn);
        } catch (error) {
          if (error !== CRASH) throw error;
          await crashed.park();
          return store.transaction(fn);
        }
      }) as never,
  );
  return { store: wrapped, crashed };
}

/** The answer transaction throws once, after its task and input writes. */
function failAnswerOnce(store: AssistantStore): AssistantStore {
  let armed = true;
  return withTx(store, (tx) =>
    overlay(tx, {
      conversations: overlay(tx.conversations, {
        appendMessage: (input: { metadata?: Record<string, unknown> }) => {
          if (armed && input.metadata?.["humanAnswer"] === true) {
            armed = false;
            throw new Error("simulated crash during answer");
          }
          return (
            tx.conversations.appendMessage as (i: unknown) => Promise<unknown>
          ).call(tx.conversations, input);
        },
      }),
    }),
  );
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

const ADAPTERS: Adapter[] = ["memory", "sqlite"];

for (const adapter of ADAPTERS) {
  const label =
    adapter === "memory" ? "memory" : 'sqlite (file, reopened on "restart")';
  describe(`e2e sessions crash matrix — ${label}`, () => {
    it("C1: crash before the model request; attempt 2 answers once", async () => {
      const world = await createWorld(adapter, [
        { park: "start", turn: text(ZOMBIE_ANSWER) },
        { turn: text(ANSWER_2) },
      ]);
      const a = await startNode(world, { ownerId: "owner-A" });
      await seedSession(a);
      const turn = await sendTurn(a, "hello");
      await world.provider.gate(1).whenParked();
      const crashed = await world.store.tasks.getTask(turn.turnId);
      expect([crashed?.status, crashed?.attemptCount]).toEqual(["running", 1]);

      const b = await crashAndRecover(world, "owner-B");
      await waitTask(world, turn.turnId, "completed", 2);
      expect(world.provider.calls).toHaveLength(2);

      const messages = await messagesOf(world);
      expect(messages.map((m) => m.role)).toEqual(["user", "assistant"]);
      const placeholder = placeholderOf(messages, turn.assistantMessageId);
      expect(placeholder?.content).toBe(ANSWER_2);
      expect(placeholder?.metadata["placeholder"]).toBe(false);

      const eventsBefore = (await eventsOf(world, turn.turnId)).length;
      const messagesBefore = messages.length;
      await wakeZombie(world, a);
      expect(world.provider.resumed).toBe(1);
      expect(world.provider.calls).toHaveLength(2);
      await assertEndState(world, {
        runs: { [turn.turnId]: { status: "completed", attempts: 2 } },
        zombie: {
          runId: turn.turnId,
          executions: a.executions,
          eventsBefore,
          messagesBefore,
        },
      });
      expect(b.executions).toHaveLength(1);
    });

    it("C2: mid-stream crash; the placeholder holds attempt 2's answer only", async () => {
      const world = await createWorld(adapter, [
        { park: "beforeCompleted", turn: text(ZOMBIE_ANSWER) },
        { turn: text(ANSWER_2) },
      ]);
      const a = await startNode(world, { ownerId: "owner-A" });
      await seedSession(a);
      const turn = await sendTurn(a, "hello");
      await world.provider.gate(1).whenParked();
      const stranded = placeholderOf(
        await messagesOf(world),
        turn.assistantMessageId,
      );
      // The delta is in the durable log; the placeholder row is not flushed
      // per delta, so it is still open (and empty) when the process dies.
      expect(stranded?.metadata["placeholder"]).toBe(true);
      expect(
        (await eventsOf(world, turn.turnId)).flatMap((e) =>
          e.type === "run.message.delta" ? [e.data.delta] : [],
        ),
      ).toEqual([ZOMBIE_ANSWER]);

      await crashAndRecover(world, "owner-B");
      await waitTask(world, turn.turnId, "completed", 2);

      const messages = await messagesOf(world);
      const placeholder = placeholderOf(messages, turn.assistantMessageId);
      // Not "ZOMBIE-ANSWER" + attempt 2's text: attempt 2 starts from empty.
      expect(placeholder?.content).toBe(ANSWER_2);
      expect(placeholder?.metadata["placeholder"]).toBe(false);
      const deltas = (await eventsOf(world, turn.turnId)).flatMap((e) =>
        e.type === "run.message.delta" ? [e.data.delta] : [],
      );
      expect(deltas).toEqual([ZOMBIE_ANSWER, ANSWER_2]);

      const eventsBefore = (await eventsOf(world, turn.turnId)).length;
      const messagesBefore = messages.length;
      await wakeZombie(world, a);
      expect(world.provider.resumed).toBe(1);
      await assertEndState(world, {
        runs: { [turn.turnId]: { status: "completed", attempts: 2 } },
        zombie: {
          runId: turn.turnId,
          executions: a.executions,
          eventsBefore,
          messagesBefore,
        },
      });
    });

    it("C5: an uncertain effect requests approval before another provider call", async () => {
      const world = await createWorld(adapter, [
        { turn: toolCall("call-b1", "bump") },
        { turn: text(ANSWER_2) },
      ]);
      world.blockFirstBump = true;
      const a = await startNode(world, { ownerId: "owner-A" });
      await seedSession(a);
      const turn = await sendTurn(a, "bump it");
      await world.toolGate.whenParked();
      // The side effect happened; no `run.tool.*` event was logged for it.
      expect(world.bumps).toBe(1);
      const before = await eventsOf(world, turn.turnId);
      expect(before.some((e) => e.type === "run.tool.succeeded")).toBe(false);

      const b = await crashAndRecover(world, "owner-B");
      await waitTask(world, turn.turnId, "waiting_approval", 2);

      // Recovery requests a new decision for the original call before asking
      // the provider to continue. No recorded result means uncertainty.
      expect(world.bumps).toBe(1);
      expect(world.provider.calls).toHaveLength(1);
      const waiting = (await b.service.getState(SESSION)).waiting;
      expect(waiting?.toolCallId).toBe("call-b1");
      expect(waiting?.content).toMatchObject({
        kind: "approval",
        toolName: "bump",
      });
      expect(JSON.stringify(waiting?.content)).toContain("uncertain");
      // ...in memory only: no tool record was ever persisted for it.
      const messages = await messagesOf(world);
      expect(messages.filter((m) => m.role === "tool")).toHaveLength(0);

      const eventsBefore = (await eventsOf(world, turn.turnId)).length;
      const messagesBefore = messages.length;
      await wakeZombie(world, a);
      // The zombie's tool returned late; its result landed nowhere and it made
      // no further model call. The side effect count is still 1.
      expect(world.bumps).toBe(1);
      expect(world.provider.calls).toHaveLength(1);
      await assertEndState(world, {
        runs: { [turn.turnId]: { status: "waiting_approval", attempts: 2 } },
        zombie: {
          runId: turn.turnId,
          executions: a.executions,
          eventsBefore,
          messagesBefore,
        },
      });
    });

    it("C5b: fresh approval admits one reattempt of the original uncertain call", async () => {
      const world = await createWorld(adapter, [
        { turn: toolCall("call-b1", "bump") },
        { turn: text(ANSWER_2) },
      ]);
      world.blockFirstBump = true;
      const a = await startNode(world, { ownerId: "owner-A" });
      await seedSession(a);
      const turn = await sendTurn(a, "bump it");
      await world.toolGate.whenParked();

      const b = await crashAndRecover(world, "owner-B");
      // The original call parks before the provider can propose any reissue.
      await waitTask(world, turn.turnId, "waiting_approval", 2);
      expect(world.bumps).toBe(1);
      const waiting = (await b.service.getState(SESSION)).waiting;
      expect(waiting?.content).toMatchObject({ toolName: "bump" });
      expect(waiting?.toolCallId).toBe("call-b1");
      expect(world.provider.calls).toHaveLength(1);

      await b.service.approve(waiting!.inputId, "ans-1");
      // Attempt 3: the approved call executes ahead of the model, and the turn
      // finishes on the answer the script had queued.
      await waitTask(world, turn.turnId, "completed", 3);
      expect(world.provider.calls).toHaveLength(2);

      // Exactly once beyond the crash: the interrupted bump, plus the approved
      // re-issue. Nothing ran twice behind anyone's back.
      expect(world.bumps).toBe(2);
      const messages = await messagesOf(world);
      expect(
        messages.filter((m) => m.role === "tool").map((m) => m.toolCallId),
      ).toEqual(["call-b1"]);

      const eventsBefore = (await eventsOf(world, turn.turnId)).length;
      const messagesBefore = messages.length;
      await wakeZombie(world, a);
      expect(world.bumps).toBe(2);
      await assertEndState(world, {
        runs: { [turn.turnId]: { status: "completed", attempts: 3 } },
        zombie: {
          runId: turn.turnId,
          executions: a.executions,
          eventsBefore,
          messagesBefore,
        },
      });
    });

    it("C6: a placed steer survives a crash in step 2 exactly once", async () => {
      const STEER = "STEER-TEXT";
      const world = await createWorld(adapter, [
        { turn: toolCall("call-e1", "echo_steer") },
        { park: "start", turn: text(ZOMBIE_ANSWER) },
        { turn: text(ANSWER_2) },
      ]);
      const a = await startNode(world, { ownerId: "owner-A" });
      world.onEcho = async () => {
        // Created while turn 1 is running (inside its tool batch).
        await world.nodes[0]!.service.steer(SESSION, {
          inputId: "steer-1",
          content: STEER,
        });
      };
      await seedSession(a);
      const turn = await sendTurn(a, "go");
      await world.provider.gate(2).whenParked();

      // Step 2's request (the doomed one) already contains the steer once.
      expect(countUser(world.provider.calls[1]!, STEER)).toBe(1);
      const placed = (await eventsOf(world, turn.turnId)).filter(
        (e) => e.type === "run.input.applied",
      );
      expect(placed).toHaveLength(1);
      expect((await world.store.inputs!.getInput("steer-1"))?.status).toBe(
        "applied",
      );

      await crashAndRecover(world, "owner-B");
      await waitTask(world, turn.turnId, "completed", 2);

      // Attempt 2's step-2 request: the steer once, not zero, not twice.
      expect(world.provider.calls).toHaveLength(3);
      expect(countUser(world.provider.calls[2]!, STEER)).toBe(1);
      const messages = await messagesOf(world);
      expect(
        messages.filter((m) => m.metadata["steering"] === true),
      ).toHaveLength(1);
      const input = await world.store.inputs!.getInput("steer-1");
      expect(input?.status).toBe("applied");
      expect(input?.appliedTaskId).toBe(turn.turnId);
      expect(
        (await eventsOf(world, turn.turnId)).filter(
          (e) => e.type === "run.input.applied",
        ),
      ).toHaveLength(1);

      const eventsBefore = (await eventsOf(world, turn.turnId)).length;
      const messagesBefore = messages.length;
      await wakeZombie(world, a);
      await assertEndState(world, {
        runs: { [turn.turnId]: { status: "completed", attempts: 2 } },
        zombie: {
          runId: turn.turnId,
          executions: a.executions,
          eventsBefore,
          messagesBefore,
        },
      });
    });

    describe("human input", () => {
      const ASK = (args = '{"prompt":"Pick one"}'): CallPlan[] => [
        { turn: toolCall("call-u1", "request_user_input", args) },
        { turn: text("DONE") },
      ];
      const requestIdOf = (turnId: string) => humanRequestId(turnId, "call-u1");

      it("C7: quit while waiting; the answer after restart resumes exactly", async () => {
        const world = await createWorld(adapter, ASK());
        const a = await startNode(world, { ownerId: "owner-A" });
        await seedSession(a);
        const turn = await sendTurn(a, "ask me");
        await waitTask(world, turn.turnId, "waiting_approval", 1);
        expect((await a.service.getState(SESSION)).waiting?.inputId).toBe(
          requestIdOf(turn.turnId),
        );

        // Restart: no lease or attempt in flight.
        await quit(world, a);
        const b = await startNode(world, { ownerId: "owner-B" });
        await b.taskRunner.recover();
        expect((await world.store.tasks.getTask(turn.turnId))?.status).toBe(
          "waiting_approval",
        );
        expect(world.provider.calls).toHaveLength(1);

        const requestId = requestIdOf(turn.turnId);
        const receipt = await b.service.answer({
          requestId,
          answerId: "ans-1",
          response: { choice: "b" },
        });
        expect(receipt.duplicate).toBe(false);
        await waitTask(world, turn.turnId, "completed", 2);

        expect(world.provider.calls).toHaveLength(2);
        const answered = world.provider.calls[1]!.find(
          (m) => m.role === "tool" && m.toolCallId === "call-u1",
        );
        expect(JSON.parse(answered!.content as string).data.answer).toEqual({
          choice: "b",
        });

        const again = await b.service.answer({
          requestId,
          answerId: "ans-1",
          response: { choice: "b" },
        });
        expect(again.duplicate).toBe(true);
        expect(again.messageId).toBe(receipt.messageId);
        expect(
          await codeOf(
            b.service.answer({
              requestId,
              answerId: "ans-1",
              response: { choice: "c" },
            }),
          ),
        ).toBe("answer_conflict");
        expect(
          await codeOf(
            b.service.answer({
              requestId,
              answerId: "ans-2",
              response: { choice: "b" },
            }),
          ),
        ).toBe("answer_conflict");

        const final = placeholderOf(
          await messagesOf(world),
          turn.assistantMessageId,
        );
        expect(final?.content).toBe("DONE");
        await assertEndState(world, {
          runs: { [turn.turnId]: { status: "completed", attempts: 2 } },
        });
      });

      it("C7b: crash between run.waiting and park; recovery re-parks without a model call", async () => {
        const world = await createWorld(adapter, ASK());
        const crash = crashOnPark(world.store);
        const a = await startNode(world, {
          ownerId: "owner-A",
          wrap: () => crash.store,
        });
        await seedSession(a);
        const turn = await sendTurn(a, "ask me");
        await crash.crashed.whenParked();
        // run.waiting and its request are durable; the task never parked.
        const stuck = await world.store.tasks.getTask(turn.turnId);
        expect([stuck?.status, stuck?.attemptCount]).toEqual(["running", 1]);
        const pending = await world.store.inputs!.listInputs({
          sessionId: SESSION,
          kinds: ["human_request"],
          statuses: ["pending"],
        });
        expect(pending.map((r) => r.inputId)).toEqual([
          requestIdOf(turn.turnId),
        ]);
        expect(world.provider.calls).toHaveLength(1);

        const b = await crashAndRecover(world, "owner-B");
        await waitTask(world, turn.turnId, "waiting_approval", 2);
        // The resume guard parked it again: no second provider call.
        expect(world.provider.calls).toHaveLength(1);
        expect(
          (
            await world.store.inputs!.listInputs({
              sessionId: SESSION,
              kinds: ["human_request"],
            })
          ).length,
        ).toBe(1);

        await b.service.answer({
          requestId: requestIdOf(turn.turnId),
          answerId: "ans-1",
          response: "yes",
        });
        await waitTask(world, turn.turnId, "completed", 3);
        expect(world.provider.calls).toHaveLength(2);
        const answered = world.provider.calls[1]!.find(
          (m) => m.role === "tool" && m.toolCallId === "call-u1",
        );
        expect(JSON.parse(answered!.content as string).data.answer).toBe("yes");

        const messages = await messagesOf(world);
        const eventsBefore = (await eventsOf(world, turn.turnId)).length;
        const messagesBefore = messages.length;
        // The zombie's late park retries with its stale lease and is refused.
        crash.crashed.release();
        await a.stopWorker();
        await assertEndState(world, {
          runs: { [turn.turnId]: { status: "completed", attempts: 3 } },
          zombie: {
            runId: turn.turnId,
            executions: a.executions,
            eventsBefore,
            messagesBefore,
          },
        });
      });

      it("C8: crash during answer", async () => {
        const world = await createWorld(adapter, ASK());
        const a = await startNode(world, { ownerId: "owner-A" });
        await seedSession(a);
        const turn = await sendTurn(a, "ask me");
        await waitTask(world, turn.turnId, "waiting_approval", 1);
        await quit(world, a);
        // No worker yet: nothing may claim a task the failed answer re-queued.
        const b = await startNode(world, {
          ownerId: "owner-B",
          wrap: failAnswerOnce,
          startWorker: false,
        });
        const requestId = requestIdOf(turn.turnId);
        const answer = {
          requestId,
          answerId: "ans-1",
          response: { choice: "b" },
        };
        await expect(b.service.answer(answer)).rejects.toThrow(
          "simulated crash during answer",
        );
        const task = await world.store.tasks.getTask(turn.turnId);
        const request = await world.store.inputs!.getInput(requestId);

        if (adapter === "sqlite") {
          // Rolled back: still waiting, still unanswered, no answer record.
          expect(task?.status).toBe("waiting_approval");
          expect(request?.status).toBe("pending");
          expect(
            (await messagesOf(world)).filter((m) => m.role === "tool"),
          ).toHaveLength(0);
          await b.startWorker();
          const receipt = await b.service.answer(answer);
          expect(receipt.duplicate).toBe(false);
          await waitTask(world, turn.turnId, "completed", 2);
          expect(
            world.provider.calls[1]!.some(
              (m) =>
                m.role === "tool" &&
                m.toolCallId === "call-u1" &&
                JSON.parse(m.content as string).data.answer.choice === "b",
            ),
          ).toBe(true);
          await assertEndState(world, {
            runs: { [turn.turnId]: { status: "completed", attempts: 2 } },
          });
          return;
        }
        // memory has no rollback: the task and input transitions stuck and the
        // answer record never landed. A retry of the SAME answer repairs it —
        // writes the record under the receipt's promised id — rather than
        // refusing, so the resumed turn sees the human's answer, not a
        // synthetic missing result.
        expect(task?.status).toBe("queued");
        expect(request?.status).toBe("answered");
        expect(request?.appliedMessageId).toBeDefined();
        expect(
          (await messagesOf(world)).filter((m) => m.role === "tool"),
        ).toHaveLength(0);
        const repaired = await b.service.answer(answer);
        expect(repaired.duplicate).toBe(false);
        expect(repaired.messageId).toBe(request?.appliedMessageId as string);
        // A DIFFERENT answer id is still a conflict, repaired or not.
        expect(
          await codeOf(b.service.answer({ ...answer, answerId: "ans-2" })),
        ).toBe("answer_conflict");
        await b.startWorker();
        await waitTask(world, turn.turnId, "completed", 2);
        expect(
          world.provider.calls[1]!.some(
            (m) =>
              m.role === "tool" &&
              m.toolCallId === "call-u1" &&
              JSON.parse(m.content as string).data.answer.choice === "b",
          ),
        ).toBe(true);
        await assertEndState(world, {
          runs: { [turn.turnId]: { status: "completed", attempts: 2 } },
        });
      });

      it("stale: an answer after cancel is refused", async () => {
        const world = await createWorld(adapter, ASK());
        const a = await startNode(world, { ownerId: "owner-A" });
        await seedSession(a);
        const turn = await sendTurn(a, "ask me");
        await waitTask(world, turn.turnId, "waiting_approval", 1);
        await quit(world, a);
        const b = await startNode(world, { ownerId: "owner-B" });

        await b.service.cancel(SESSION, turn.turnId);
        expect((await world.store.tasks.getTask(turn.turnId))?.status).toBe(
          "cancelled",
        );
        expect(
          await codeOf(
            b.service.answer({
              requestId: requestIdOf(turn.turnId),
              answerId: "ans-1",
              response: "late",
            }),
          ),
        ).toBe("answer_stale");
        expect(world.provider.calls).toHaveLength(1);
        expect(
          (await messagesOf(world)).filter((m) => m.role === "tool"),
        ).toHaveLength(0);
        await assertEndState(world, {
          runs: { [turn.turnId]: { status: "cancelled", attempts: 1 } },
        });
      });

      it("stale: an answer after expiry is refused", async () => {
        const world = await createWorld(
          adapter,
          ASK('{"prompt":"Quick","expiresInMs":5000}'),
        );
        const a = await startNode(world, { ownerId: "owner-A" });
        await seedSession(a);
        const turn = await sendTurn(a, "ask me");
        await waitTask(world, turn.turnId, "waiting_approval", 1);
        await quit(world, a);
        const b = await startNode(world, { ownerId: "owner-B" });

        world.clock.advance(5_001);
        expect(
          await codeOf(
            b.service.answer({
              requestId: requestIdOf(turn.turnId),
              answerId: "ans-1",
              response: "late",
            }),
          ),
        ).toBe("answer_stale");
        // Refused cleanly: still parked, request untouched, no model call.
        expect((await world.store.tasks.getTask(turn.turnId))?.status).toBe(
          "waiting_approval",
        );
        expect(world.provider.calls).toHaveLength(1);
        await assertEndState(world, {
          runs: { [turn.turnId]: { status: "waiting_approval", attempts: 1 } },
        });
      });
    });

    it("follow-up across restart: converted once, turn 2 runs on the new runner", async () => {
      let a!: Node;
      const world = await createWorld(adapter, [
        {
          turn: text("T1"),
          before: async () => {
            const queued = await a.service.followUp(SESSION, {
              inputId: "fu-1",
              content: "FOLLOW",
            });
            expect(queued.kind).toBe("queued");
          },
        },
        { turn: text("T2") },
      ]);
      // No worker on A: turn 1 is driven by hand so turn 2, once staged by
      // turn 1's settlement, cannot be claimed before the restart.
      a = await startNode(world, { ownerId: "owner-A", startWorker: false });
      await seedSession(a);
      const turn1 = await sendTurn(a, "first");
      const claimed = await world.store.tasks.claimNext({
        ownerId: "owner-A",
        now: world.clock.now(),
        scopesBusy: [],
      });
      expect(claimed?.task.taskId).toBe(turn1.turnId);
      await a.turnRunner.execute({
        taskId: turn1.turnId,
        attemptId: claimed!.attempt.attemptId,
        leaseToken: claimed!.lease.leaseToken,
        signal: new AbortController().signal,
      });

      expect((await world.store.tasks.getTask(turn1.turnId))?.status).toBe(
        "completed",
      );
      const converted = await world.store.inputs!.getInput("fu-1");
      expect(converted?.status).toBe("converted");
      const turn2Id = converted!.appliedTaskId!;
      expect(turn2Id).not.toBe(turn1.turnId);
      const queuedTurn = await world.store.tasks.getTask(turn2Id);
      expect([queuedTurn?.status, queuedTurn?.attemptCount]).toEqual([
        "queued",
        0,
      ]);
      expect(world.provider.calls).toHaveLength(1);

      await quit(world, a);
      const b = await startNode(world, { ownerId: "owner-B" });
      await b.taskRunner.recover();
      await waitTask(world, turn2Id, "completed", 1);

      expect(world.provider.calls).toHaveLength(2);
      expect(countUser(world.provider.calls[1]!, "FOLLOW")).toBe(1);
      const input = await world.store.inputs!.getInput("fu-1");
      expect(input?.status).toBe("converted");
      expect(input?.appliedTaskId).toBe(turn2Id);
      // No second conversion: nothing is left to flush and only two turns exist.
      expect(await b.service.flushQueue(SESSION)).toBeUndefined();
      const turns = (await world.store.tasks.listByScope(SESSION)).filter(
        (t) => t.kind === "chat.turn",
      );
      expect(turns.map((t) => t.taskId).sort()).toEqual(
        [turn1.turnId, turn2Id].sort(),
      );
      const messages = await messagesOf(world);
      expect(
        messages.filter((m) => m.role === "user" && m.content === "FOLLOW"),
      ).toHaveLength(1);
      await assertEndState(world, {
        runs: {
          [turn1.turnId]: { status: "completed", attempts: 1 },
          [turn2Id]: { status: "completed", attempts: 1 },
        },
      });
    });
  });
}
