import { describe, expect, it } from "bun:test";
import {
  type AgentKitHostError,
  CHAT_TURN_TASK_KIND,
  ExecutorNotFoundError,
  ExecutorRegistry,
  RecordNotFoundError,
  TaskService,
  createDispatchingWorker,
  type TaskExecutionContext,
  type TaskExecutor,
  type TaskRecord,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/** Records every context it was handed, so dispatch can be asserted on. */
class RecordingExecutor implements TaskExecutor {
  readonly seen: TaskExecutionContext[] = [];

  constructor(readonly kind: string) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    this.seen.push(ctx);
  }
}

/**
 * Fans one child out from inside `execute`, so the wiring is exercised where an
 * executor would actually use it rather than through a captured closure.
 */
class SpawningExecutor implements TaskExecutor {
  readonly kind = "demo.fanout";
  /** What `ctx.spawnChild` was on the dispatch — the wiring assertion. */
  offered: boolean | undefined;
  spawned: TaskRecord | null = null;

  async execute(ctx: TaskExecutionContext): Promise<void> {
    this.offered = ctx.spawnChild !== undefined;
    if (!ctx.spawnChild) return;
    this.spawned = await ctx.spawnChild({
      taskId: "task-child",
      kind: "demo.echo",
      scopeId: "scope-child",
      payload: { text: "child" },
    });
  }
}

/** A persisted `queued` task plus the attempt + lease a claim would grant. */
async function seedTask(
  f: TestHarness,
  taskId: string,
  kind: string,
): Promise<{ attemptId: string; leaseToken: string }> {
  await f.store.tasks.createTask({
    taskId,
    kind,
    scopeId: "scope-1",
    payload: { hello: "world" },
  });
  const attempt = await f.store.tasks.createAttempt({
    attemptId: f.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId: attempt.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  return { attemptId: attempt.attemptId, leaseToken: lease.leaseToken };
}

describe("ExecutorRegistry", () => {
  it("refuses a second executor for a kind that is already registered", () => {
    const registry = new ExecutorRegistry();
    registry.register(new RecordingExecutor("demo.echo"));
    let caught: AgentKitHostError | undefined;
    try {
      registry.register(new RecordingExecutor("demo.echo"));
    } catch (err) {
      caught = err as AgentKitHostError;
    }
    expect(caught?.code).toBe("duplicate_executor_kind");
    expect(caught?.details).toEqual({ kind: "demo.echo" });
    // The first registration is the one that stands.
    expect(registry.kinds()).toEqual(["demo.echo"]);
  });

  it("reports every registered kind, in registration order", () => {
    const registry = new ExecutorRegistry();
    registry.register(new RecordingExecutor(CHAT_TURN_TASK_KIND));
    registry.register(new RecordingExecutor("index.rebuild"));
    expect(registry.kinds()).toEqual([CHAT_TURN_TASK_KIND, "index.rebuild"]);
    expect(registry.get("index.rebuild")?.kind).toBe("index.rebuild");
    expect(registry.get("nobody.registered")).toBeUndefined();
  });
});

describe("createDispatchingWorker", () => {
  it("routes a task to the executor for its kind, with the loaded record", async () => {
    const f = createHarness();
    const chat = new RecordingExecutor(CHAT_TURN_TASK_KIND);
    const echo = new RecordingExecutor("demo.echo");
    const registry = new ExecutorRegistry();
    registry.register(chat);
    registry.register(echo);
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
    });

    const seeded = await seedTask(f, "task-echo", "demo.echo");
    const signal = new AbortController().signal;
    await worker.execute({ taskId: "task-echo", ...seeded, signal });

    expect(chat.seen).toHaveLength(0);
    expect(echo.seen).toHaveLength(1);
    // The record travels WITH the dispatch: an executor never re-fetches, so
    // it cannot disagree with the guard that just ran.
    expect(echo.seen[0]?.task.taskId).toBe("task-echo");
    expect(echo.seen[0]?.task.kind).toBe("demo.echo");
    expect(echo.seen[0]?.task.payload).toEqual({ hello: "world" });
    expect(echo.seen[0]?.attemptId).toBe(seeded.attemptId);
    expect(echo.seen[0]?.leaseToken).toBe(seeded.leaseToken);
  });

  it("transitions a queued task to running before executing, and hands over the fresh record", async () => {
    const f = createHarness();
    const echo = new RecordingExecutor("demo.echo");
    const registry = new ExecutorRegistry();
    registry.register(echo);
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
    });

    const seeded = await seedTask(f, "task-queued", "demo.echo");
    expect((await f.store.tasks.getTask("task-queued"))?.status).toBe("queued");

    await worker.execute({
      taskId: "task-queued",
      ...seeded,
      signal: new AbortController().signal,
    });

    const stored = await f.store.tasks.getTask("task-queued");
    expect(stored?.status).toBe("running");
    expect(stored?.startedAt).toBe(f.clock.nowIso());
    expect(echo.seen[0]?.task.status).toBe("running");
  });

  it("leaves an already-running task alone — claimNext did that transition", async () => {
    const f = createHarness();
    const echo = new RecordingExecutor("demo.echo");
    const registry = new ExecutorRegistry();
    registry.register(echo);
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
    });

    const seeded = await seedTask(f, "task-running", "demo.echo");
    await f.store.tasks.transitionTask("task-running", ["queued"], "running", {
      startedAt: "2020-01-01T00:00:00.000Z",
    });

    await worker.execute({
      taskId: "task-running",
      ...seeded,
      signal: new AbortController().signal,
    });

    // A second transition would have thrown (running → running is illegal);
    // the untouched startedAt proves nothing was rewritten.
    expect((await f.store.tasks.getTask("task-running"))?.startedAt).toBe(
      "2020-01-01T00:00:00.000Z",
    );
    expect(echo.seen).toHaveLength(1);
  });

  it("refuses a task that already reached a terminal state", async () => {
    const f = createHarness();
    const echo = new RecordingExecutor("demo.echo");
    const registry = new ExecutorRegistry();
    registry.register(echo);
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
    });

    const seeded = await seedTask(f, "task-done", "demo.echo");
    await f.store.tasks.transitionTask("task-done", ["queued"], "cancelled");

    let caught: AgentKitHostError | undefined;
    try {
      await worker.execute({
        taskId: "task-done",
        ...seeded,
        signal: new AbortController().signal,
      });
    } catch (err) {
      caught = err as AgentKitHostError;
    }
    expect(caught?.code).toBe("task_not_executable");
    expect(caught?.message).toMatch(/only queued or running/);
    expect(echo.seen).toHaveLength(0);
  });

  it("throws RecordNotFoundError for a taskId with no row", async () => {
    const f = createHarness();
    const worker = createDispatchingWorker(new ExecutorRegistry(), {
      store: f.store,
      clock: f.clock,
    });
    await expect(
      worker.execute({
        taskId: "never-persisted",
        attemptId: "att-1",
        leaseToken: "lease-1",
        signal: new AbortController().signal,
      }),
    ).rejects.toThrow(RecordNotFoundError);
  });

  it("throws ExecutorNotFoundError, naming the kind, when nothing handles it", async () => {
    const f = createHarness();
    const registry = new ExecutorRegistry();
    registry.register(new RecordingExecutor(CHAT_TURN_TASK_KIND));
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
    });

    const seeded = await seedTask(f, "task-orphan", "nobody.registered");
    let caught: ExecutorNotFoundError | undefined;
    try {
      await worker.execute({
        taskId: "task-orphan",
        ...seeded,
        signal: new AbortController().signal,
      });
    } catch (err) {
      caught = err as ExecutorNotFoundError;
    }
    expect(caught).toBeInstanceOf(ExecutorNotFoundError);
    expect(caught?.code).toBe("executor_not_found");
    expect(caught?.details).toEqual({
      taskId: "task-orphan",
      kind: "nobody.registered",
    });
    // The guard still ran first: the task is `running`, so the queue's
    // settleThrown can land it failed rather than leaving it claimable.
    expect((await f.store.tasks.getTask("task-orphan"))?.status).toBe(
      "running",
    );
  });
});

describe("createDispatchingWorker — spawnChild", () => {
  it("presets the spawning task as the child's parent, and submits through the queue", async () => {
    const f = createHarness();
    const fanout = new SpawningExecutor();
    const registry = new ExecutorRegistry();
    registry.register(fanout);
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
      taskService: new TaskService({
        store: f.store,
        taskRunner: f.taskRunner,
        ids: f.ids,
        clock: f.clock,
      }),
    });

    const seeded = await seedTask(f, "task-parent", "demo.fanout");
    await worker.execute({
      taskId: "task-parent",
      ...seeded,
      signal: new AbortController().signal,
    });

    expect(fanout.offered).toBe(true);
    // Lineage comes from the dispatcher, not from the executor's input — an
    // executor cannot spawn under someone else's parent, or forget its own.
    expect(fanout.spawned?.parentTaskId).toBe("task-parent");
    expect((await f.store.tasks.getTask("task-child"))?.parentTaskId).toBe(
      "task-parent",
    );
    // A spawn is a SUBMIT: the row is written AND the queue is told, or the
    // child would sit there until some unrelated poke woke the loop.
    expect(f.taskRunner.enqueued.map((input) => input.taskId)).toEqual([
      "task-child",
    ]);
  });

  it("leaves spawnChild undefined when the dispatcher was given no TaskService", async () => {
    const f = createHarness();
    const fanout = new SpawningExecutor();
    const registry = new ExecutorRegistry();
    registry.register(fanout);
    const worker = createDispatchingWorker(registry, {
      store: f.store,
      clock: f.clock,
    });

    const seeded = await seedTask(f, "task-parent-alone", "demo.fanout");
    await worker.execute({
      taskId: "task-parent-alone",
      ...seeded,
      signal: new AbortController().signal,
    });

    // Absent, not a stub: an executor that needs to fan out sees the wiring gap
    // instead of writing children nobody will ever claim.
    expect(fanout.offered).toBe(false);
    expect(fanout.spawned).toBeNull();
    expect(await f.store.tasks.getTask("task-child")).toBeNull();
    expect(f.taskRunner.enqueued).toEqual([]);
  });
});
