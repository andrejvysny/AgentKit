/**
 * Resumable interrupt (distinct from cancel): per turn via
 * `AgentSessionService.interrupt`, and on clean shutdown via
 * `shutdownMode: "interrupt"`. An interrupted turn is parked, not terminal: no
 * terminal event, no poison, and `resume` continues the SAME task with the next
 * attempt on the same event stream.
 */
import { afterEach, describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import {
  AgentSessionService,
  ChatTurnExecutor,
  ExecutionInterruptedError,
  ExecutorRegistry,
  TaskInterruptedError,
  TaskService,
  TurnRunner,
  createDispatchingWorker,
  defaultClock,
  defaultIds,
  type AssistantStore,
  type ToolSetContributor,
  type WorkerHandle,
} from "@agentkit/host";
import { MockProviderClient, type MockTurn } from "@agentkit/testing";
import { SingleProcessTaskRunner } from "../src/index.js";
import { waitFor } from "./support/task-runner-harness.js";

const SESSION = "s1";

class Gate {
  private open!: () => void;
  private readonly opened = new Promise<void>((resolve) => {
    this.open = resolve;
  });
  private announce!: () => void;
  private readonly parked = new Promise<void>((resolve) => {
    this.announce = resolve;
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
  /** Park before answering; the abort signal cannot reach this wait. */
  park?: boolean;
}

class PlannedProvider implements AiProviderClient {
  readonly id = "planned";
  readonly kind = "openai-compatible" as const;
  readonly calls: AiChatMessage[][] = [];
  private readonly gates = new Map<number, Gate>();
  constructor(private readonly plan: CallPlan[]) {}
  gate(n: number): Gate {
    let gate = this.gates.get(n);
    if (!gate) {
      gate = new Gate();
      this.gates.set(n, gate);
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
    if (entry.park) await this.gate(n).park();
    const inner = new MockProviderClient();
    inner.emitUsage = true;
    inner.setScript([entry.turn]);
    yield* inner.streamChat(input);
  }
}

const text = (content: string): MockTurn => ({
  steps: [{ kind: "text", content }],
});
const toolCall = (toolCallId: string, name: string): MockTurn => ({
  steps: [{ kind: "tool_call", toolCallId, name, argumentsJson: "{}" }],
});

interface World {
  store: MemoryAssistantStore;
  provider: PlannedProvider;
  toolGate: Gate;
  /** Times the `bump` tool body started. */
  bumps: number;
  /** The effect `bump` declares; "write" makes an interrupted call uncertain. */
  effect: "read" | "write";
  runners: SingleProcessTaskRunner[];
  handles: WorkerHandle[];
}

let worlds: World[] = [];

afterEach(async () => {
  for (const world of worlds) {
    world.provider.releaseAll();
    world.toolGate.release();
    for (const handle of world.handles) await handle.stop();
  }
  worlds = [];
});

function toolContributor(world: World): ToolSetContributor {
  const tool: AiTool = {
    definition: {
      name: "bump",
      version: "1.0.0",
      effect: world.effect,
      capability: "bump",
      description: "bump",
      inputSchema: { type: "object", properties: {} },
    },
    execute: async (ctx: { limits: never }) => {
      world.bumps += 1;
      if (world.bumps === 1) await world.toolGate.park();
      return {
        ok: true as const,
        data: { n: world.bumps },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
        summary: "bumped",
      };
    },
  } as unknown as AiTool;
  return { namespace: "test", contribute: async () => [tool] };
}

async function createWorld(
  plan: CallPlan[],
  effect: "read" | "write" = "read",
): Promise<World> {
  const world: World = {
    store: new MemoryAssistantStore(),
    provider: new PlannedProvider(plan),
    toolGate: new Gate(),
    bumps: 0,
    effect,
    runners: [],
    handles: [],
  };
  worlds.push(world);
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

interface Node {
  runner: SingleProcessTaskRunner;
  service: AgentSessionService;
  start(): Promise<WorkerHandle>;
}

function node(
  world: World,
  opts: {
    shutdownMode?: "drain" | "cancel" | "interrupt";
    recoveryMode?: "automatic" | "manual";
    interruptionMode?: "immediate" | "after_execution";
    store?: AssistantStore;
  } = {},
): Node {
  const store = opts.store ?? world.store;
  const runner = new SingleProcessTaskRunner({
    store,
    pollMs: 5,
    ...(opts.shutdownMode ? { shutdownMode: opts.shutdownMode } : {}),
    ...(opts.recoveryMode ? { recoveryMode: opts.recoveryMode } : {}),
    ...(opts.interruptionMode
      ? { interruptionMode: opts.interruptionMode }
      : {}),
  });
  world.runners.push(runner);
  const turns = new TurnRunner({
    store,
    taskRunner: runner,
    providerFactory: () => world.provider,
    contributors: [toolContributor(world)],
    clock: defaultClock,
    ids: defaultIds,
  });
  const tasks = new TaskService({
    store,
    taskRunner: runner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const service = new AgentSessionService({
    store,
    turns,
    tasks,
    taskRunner: runner,
    clock: defaultClock,
    ids: defaultIds,
  });
  const registry = new ExecutorRegistry();
  registry.register(new ChatTurnExecutor(turns));
  const worker = createDispatchingWorker(registry, { store });
  return {
    runner,
    service,
    async start() {
      const handle = await runner.startWorker(worker, { concurrency: 1 });
      world.handles.push(handle);
      return handle;
    },
  };
}

async function seed(n: Node): Promise<void> {
  await n.service.createSession({
    sessionId: SESSION,
    profile: { id: "default" },
    providerId: "p1",
    model: "m-x",
  });
}

async function send(n: Node, content: string): Promise<string> {
  const result = await n.service.send(SESSION, { content });
  if (result.kind !== "turn") throw new Error("expected a turn");
  return result.turnId;
}

const statusOf = async (world: World, id: string) =>
  (await world.store.tasks.getTask(id))?.status;

async function waitStatus(
  world: World,
  id: string,
  status: string,
): Promise<void> {
  await waitFor(
    async () => (await statusOf(world, id)) === status,
    `task ${id} to be ${status}`,
    5_000,
  );
}

const eventsOf = async (world: World, id: string) =>
  (await world.store.tasks.listEvents(id)) as AiRunEvent[];

async function codeOf(promise: Promise<unknown>): Promise<string | undefined> {
  try {
    await promise;
  } catch (error) {
    return (error as { code?: string }).code;
  }
  return undefined;
}

describe("interrupt a running turn", () => {
  it("parks interrupted with no terminal event; resume runs attempt 2 on the same stream", async () => {
    const world = await createWorld([
      { turn: text("never"), park: true },
      { turn: text("DONE") },
    ]);
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.provider.gate(1).whenParked();

    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");

    const task = await world.store.tasks.getTask(turnId);
    expect(task?.poisonCount).toBe(0);
    expect(task?.attemptCount).toBe(1);
    const events = await eventsOf(world, turnId);
    expect(events.map((e) => e.type)).not.toContain("run.cancelled");
    expect(events.map((e) => e.type)).not.toContain("run.failed");

    const state = await n.service.getState(SESSION);
    expect(state.activeTurn).toEqual({ turnId, status: "interrupted" });

    await n.service.resume(SESSION, turnId);
    await waitStatus(world, turnId, "completed");
    expect((await world.store.tasks.getTask(turnId))?.attemptCount).toBe(2);
    const after = await eventsOf(world, turnId);
    expect(after.map((e) => e.seq)).toEqual(after.map((_, i) => i));
    expect(after.at(-1)?.type).toBe("run.completed");
    const turns = await n.service.listTurns(SESSION);
    expect(turns.map((t) => [t.turnId, t.status, t.attempts])).toEqual([
      [turnId, "completed", 2],
    ]);
  });

  it("pins send() while a turn is interrupted", async () => {
    const world = await createWorld([{ turn: text("never"), park: true }]);
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.provider.gate(1).whenParked();
    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");

    // An interrupted turn still holds the session: a new message is refused.
    expect(await codeOf(n.service.send(SESSION, { content: "more" }))).toBe(
      "chat_busy",
    );
    // With an inputId the message is queued behind the interrupted turn.
    const queued = await n.service.send(SESSION, {
      content: "later",
      inputId: "in1",
      whenBusy: "follow_up",
    });
    expect(queued.kind).toBe("queued");
  });

  it("cancel of an interrupted turn lands cancelled; resume of a non-interrupted turn is invalid_state", async () => {
    const world = await createWorld([{ turn: text("never"), park: true }]);
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.provider.gate(1).whenParked();
    expect(await codeOf(n.service.resume(SESSION, turnId))).toBe(
      "invalid_state",
    );
    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");
    await n.service.cancel(SESSION, turnId);
    expect(await statusOf(world, turnId)).toBe("cancelled");
    expect(await codeOf(n.service.resume(SESSION, turnId))).toBe(
      "invalid_state",
    );
    expect(await codeOf(n.service.interrupt(SESSION, "no-such-turn"))).toBe(
      "not_found",
    );
  });

  it("interrupts a queued turn before any worker claims it", async () => {
    const world = await createWorld([{ turn: text("DONE") }]);
    const n = node(world);
    await seed(n);
    const turnId = await send(n, "hello");
    expect(await statusOf(world, turnId)).toBe("queued");
    await n.service.interrupt(SESSION, turnId);
    expect(await statusOf(world, turnId)).toBe("interrupted");
    await n.start();
    await Bun.sleep(50);
    expect(await statusOf(world, turnId)).toBe("interrupted");
    expect(world.provider.calls.length).toBe(0);
    await n.service.resume(SESSION, turnId);
    await waitStatus(world, turnId, "completed");
    expect((await world.store.tasks.getTask(turnId))?.attemptCount).toBe(1);
  });

  it("interrupt during a running tool: no duplicate tool call on resume", async () => {
    const world = await createWorld([
      { turn: toolCall("call-1", "bump") },
      { turn: text("DONE") },
    ]);
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.toolGate.whenParked();
    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");
    world.toolGate.release();
    await Bun.sleep(20);

    await n.service.resume(SESSION, turnId);
    await waitStatus(world, turnId, "completed");
    const messages = await world.store.conversations.listMessages(SESSION);
    // The tool was aborted mid-run, so no result was persisted. Attempt 2 follows
    // the existing recovery path: the call is NOT re-run (no duplicate side
    // effect) and the model is shown a synthetic failure for it.
    expect(world.bumps).toBe(1);
    const assistantCalls = messages
      .filter((m) => m.role === "assistant")
      .flatMap((m) => (m.toolCalls ?? []).map((c) => c.id));
    expect(assistantCalls).toEqual(["call-1"]);
    const resumedPass = world.provider.calls[1] ?? [];
    const result = resumedPass.find((m) => m.role === "tool");
    expect(String(result?.content)).toContain("tool_result_missing");
    const events = await eventsOf(world, turnId);
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
    expect((await world.store.tasks.getTask(turnId))?.poisonCount).toBe(0);
  });

  it("an interrupted WRITE call comes back uncertain and is not re-run", async () => {
    const world = await createWorld(
      [{ turn: toolCall("call-1", "bump") }, { turn: text("DONE") }],
      "write",
    );
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.toolGate.whenParked();
    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");
    world.toolGate.release();
    await Bun.sleep(20);

    await n.service.resume(SESSION, turnId);
    await waitStatus(world, turnId, "waiting_approval");
    expect(world.bumps).toBe(1);
    expect(world.provider.calls).toHaveLength(1);
    const waiting = (await n.service.getState(SESSION)).waiting!;
    expect(waiting.toolCallId).toBe("call-1");
    expect(JSON.stringify(waiting.content)).toContain("uncertain");
    await n.service.deny(waiting.inputId, "deny-uncertain");
    await waitStatus(world, turnId, "completed");
    expect(world.bumps).toBe(1);
  });
});

describe("one interrupt API: reason and mode", () => {
  it("a reasoned interrupt is still an ExecutionInterruptedError", () => {
    const error = new TaskInterruptedError({ code: "SLEEP", message: "Sleep" });
    expect(error).toBeInstanceOf(ExecutionInterruptedError);
    expect(error.name).toBe("TaskInterruptedError");
    expect(error.reason).toEqual({ code: "SLEEP", message: "Sleep" });
  });

  for (const interruptionMode of ["after_execution", "immediate"] as const) {
    it(`${interruptionMode}: a session interrupt records its reason and resumes`, async () => {
      const world = await createWorld([
        { turn: text("never"), park: true },
        { turn: text("DONE") },
      ]);
      const n = node(world, { interruptionMode });
      await seed(n);
      await n.start();
      const turnId = await send(n, "hello");
      await world.provider.gate(1).whenParked();

      const reason = { code: "APP_QUIT", message: "OneMind closed" };
      await n.service.interrupt(SESSION, turnId, reason);
      await waitStatus(world, turnId, "interrupted");
      const task = await world.store.tasks.getTask(turnId);
      expect(task?.error).toBe(JSON.stringify(reason));
      expect(task?.poisonCount).toBe(0);
      const types = (await eventsOf(world, turnId)).map((e) => e.type);
      expect(types).not.toContain("run.cancelled");
      expect(types).not.toContain("run.failed");

      world.provider.gate(1).release();
      await n.service.resume(SESSION, turnId);
      await waitStatus(world, turnId, "completed");
      expect((await world.store.tasks.getTask(turnId))?.attemptCount).toBe(2);
    });
  }

  it("an unreasoned interrupt records the default reason", async () => {
    const world = await createWorld([
      { turn: text("never"), park: true },
      { turn: text("DONE") },
    ]);
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.provider.gate(1).whenParked();
    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");
    expect((await world.store.tasks.getTask(turnId))?.error).toBe(
      JSON.stringify({ code: "INTERRUPTED", message: "execution interrupted" }),
    );
  });
});

describe("interrupt after a tool result", () => {
  it("keeps the recorded tool result; resume does not repeat the call", async () => {
    const world = await createWorld([
      { turn: toolCall("call-1", "bump") },
      { turn: text("never"), park: true },
      { turn: text("DONE") },
    ]);
    world.toolGate.release();
    const n = node(world);
    await seed(n);
    await n.start();
    const turnId = await send(n, "hello");
    await world.provider.gate(2).whenParked();
    await n.service.interrupt(SESSION, turnId);
    await waitStatus(world, turnId, "interrupted");
    await n.service.resume(SESSION, turnId);
    await waitStatus(world, turnId, "completed");

    expect(world.bumps).toBe(1);
    const messages = await world.store.conversations.listMessages(SESSION);
    const results = messages.filter(
      (m) => m.role === "tool" && m.toolCallId === "call-1",
    );
    expect(results.length).toBe(1);
    const events = await eventsOf(world, turnId);
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
  });
});

describe("shutdownMode interrupt", () => {
  for (const recoveryMode of ["automatic", "manual"] as const) {
    it(`stop() parks the turn; a fresh ${recoveryMode} runner never redispatches it`, async () => {
      const world = await createWorld([
        { turn: text("never"), park: true },
        { turn: text("DONE") },
      ]);
      const first = node(world, { shutdownMode: "interrupt" });
      await seed(first);
      const handle = await first.start();
      const turnId = await send(first, "hello");
      await world.provider.gate(1).whenParked();
      await handle.stop();
      expect(await statusOf(world, turnId)).toBe("interrupted");
      const task = await world.store.tasks.getTask(turnId);
      expect(task?.poisonCount).toBe(0);

      const second = node(world, { recoveryMode });
      await second.runner.recover();
      await second.start();
      await Bun.sleep(60);
      expect(await statusOf(world, turnId)).toBe("interrupted");
      expect(world.provider.calls.length).toBe(1);

      await second.service.resume(SESSION, turnId);
      await waitStatus(world, turnId, "completed");
      expect((await world.store.tasks.getTask(turnId))?.attemptCount).toBe(2);
    });
  }
});

describe("turns, usage and export", () => {
  it("reports two turns including tool calls without secrets", async () => {
    const world = await createWorld([
      { turn: toolCall("call-a", "bump") },
      { turn: text("first answer") },
      { turn: text("second answer") },
    ]);
    // Nothing parks: the first `bump` must return at once.
    world.toolGate.release();
    const n = node(world);
    await seed(n);
    await n.start();
    const t1 = await send(n, "one");
    await waitStatus(world, t1, "completed");
    const t2 = await send(n, "two");
    await waitStatus(world, t2, "completed");

    const turns = await n.service.listTurns(SESSION);
    expect(turns.map((t) => [t.turnId, t.status, t.attempts])).toEqual([
      [t1, "completed", 1],
      [t2, "completed", 1],
    ]);
    expect(turns[0]?.model).toBe("m-x");

    const usage = await n.service.getUsage(SESSION);
    expect(usage.turns.map((t) => [t.turnId, t.requests])).toEqual([
      [t1, 2],
      [t2, 1],
    ]);
    expect(usage.totals).toEqual({
      requests: 3,
      promptTokens: 30,
      completionTokens: 15,
      totalTokens: 45,
    });
    expect(usage.lastPromptTokens).toBe(10);
    expect(usage.contextWindowTokens).toBeGreaterThan(0);

    // A secret-bearing metadata key on a message must not be exported.
    const records = await world.store.conversations.listMessages(SESSION);
    const answer = records.find((m) => m.role === "assistant");
    if (!answer) throw new Error("no assistant record");
    await world.store.conversations.updateMessage(answer.id, {
      metadata: {
        ...answer.metadata,
        providerContinuation: "SECRET-CONTINUATION",
        encryptedReasoning: "SECRET-ENCRYPTED",
      },
    });
    const exported = await n.service.exportSession(SESSION);
    const json = JSON.stringify(exported);
    expect(JSON.parse(json)).toEqual(exported);
    expect(json).not.toContain("SECRET-");
    expect(exported.format).toBe("agentkit.session.export");
    expect(exported.sessionId).toBe(SESSION);
    expect(exported.turns.length).toBe(2);
    const calls = exported.messages.flatMap((m) =>
      (m.toolCalls ?? []).map((c) => (c as { id: string }).id),
    );
    expect(calls).toEqual(["call-a"]);
    expect(
      exported.messages.filter(
        (m) => m.role === "tool" && m.toolCallId === "call-a",
      ).length,
    ).toBe(1);
    expect(exported.messages.some((m) => m.metadata["internal"] === true)).toBe(
      true,
    );
  });
});
