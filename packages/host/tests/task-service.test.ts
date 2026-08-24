import { describe, expect, it } from "bun:test";
import {
  DuplicateTaskError,
  TaskService,
  type AssistantStore,
} from "../src/index.js";
import {
  FakeAssistantStore,
  createTestClock,
  createTestIds,
  FakeTaskRunner,
  type TestClock,
} from "./fakes.js";

/**
 * {@link FakeAssistantStore.transaction} runs the callback against the live
 * Maps and documents that it cannot roll back. This subclass snapshots the two
 * collections these tests write to and restores them on a throw — the minimum
 * needed to show that a task created through `tx` is discarded together with
 * whatever else the unit wrote, which is the property `createTask` exists to
 * make available.
 */
class RollingBackStore extends FakeAssistantStore {
  override async transaction<T>(
    fn: (tx: AssistantStore) => Promise<T>,
  ): Promise<T> {
    const tasks = new Map(this.tasks.tasks);
    const chats = new Map(this.conversations.chats);
    try {
      return await super.transaction(fn);
    } catch (err) {
      restore(this.tasks.tasks, tasks);
      restore(this.conversations.chats, chats);
      throw err;
    }
  }
}

function restore<K, V>(live: Map<K, V>, snapshot: Map<K, V>): void {
  live.clear();
  for (const [key, value] of snapshot) live.set(key, value);
}

interface Fixture {
  clock: TestClock;
  store: RollingBackStore;
  taskRunner: FakeTaskRunner;
  service: TaskService;
  /** How many times `enqueue` was actually called (the fake dedups inside). */
  enqueueCalls(): number;
}

function setup(): Fixture {
  const clock = createTestClock();
  const ids = createTestIds();
  const store = new RollingBackStore(clock, ids);
  const taskRunner = new FakeTaskRunner();
  let calls = 0;
  const inner = taskRunner.enqueue.bind(taskRunner);
  taskRunner.enqueue = async (input) => {
    calls += 1;
    await inner(input);
  };
  return {
    clock,
    store,
    taskRunner,
    service: new TaskService({ store, taskRunner, ids, clock }),
    enqueueCalls: () => calls,
  };
}

describe("TaskService.createTask", () => {
  it("composes inside the host's own transaction, and rolls back with it", async () => {
    const f = setup();
    await expect(
      f.store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-atomic" });
        await f.service.createTask(tx, {
          taskId: "task-atomic",
          kind: "demo.echo",
          scopeId: "chat-atomic",
          payload: { note: "hi" },
        });
        throw new Error("the host's own write failed");
      }),
    ).rejects.toThrow("the host's own write failed");

    // Neither half survives: that is the whole reason createTask takes a `tx`
    // instead of opening its own transaction.
    expect(await f.store.tasks.getTask("task-atomic")).toBeNull();
    expect(await f.store.conversations.getChat("chat-atomic")).toBeNull();
    // And nothing was told to run work that no longer exists.
    expect(f.enqueueCalls()).toBe(0);
  });

  it("writes the task and NEVER enqueues", async () => {
    const f = setup();
    const task = await f.service.createTask(f.store, {
      taskId: "task-1",
      kind: "demo.echo",
      scopeId: "scope-1",
      payload: { note: "hi" },
      priority: 5,
    });
    expect(task.taskId).toBe("task-1");
    expect(task.kind).toBe("demo.echo");
    expect(task.status).toBe("queued");
    expect(task.priority).toBe(5);
    expect(f.taskRunner.enqueued).toEqual([]);
  });

  it("mints a taskId when the caller does not supply one", async () => {
    const f = setup();
    const task = await f.service.createTask(f.store, {
      kind: "demo.echo",
      scopeId: "scope-1",
      payload: {},
    });
    expect(task.taskId).toBe("task-1");
    expect(await f.store.tasks.getTask("task-1")).not.toBeNull();
  });
});

describe("TaskService.submitTask", () => {
  it("creates then dispatches, in that order — never from inside the transaction", async () => {
    const f = setup();
    const enqueuedDuringTransaction: number[] = [];
    const inner = f.store.transaction.bind(f.store);
    f.store.transaction = async (fn) =>
      inner(async (tx) => {
        const result = await fn(tx);
        // Read at the last moment INSIDE the callback: an enqueue here would
        // let the claim loop join this open transaction and claim a row that
        // could still roll back.
        enqueuedDuringTransaction.push(f.taskRunner.enqueued.length);
        return result;
      });

    const task = await f.service.submitTask({
      taskId: "task-order",
      kind: "demo.echo",
      scopeId: "scope-1",
      payload: {},
      priority: 3,
    });

    expect(enqueuedDuringTransaction).toEqual([0]);
    expect(task.taskId).toBe("task-order");
    // The created record is what gets dispatched, so the queue is told the
    // scheduling fields the STORE stamped, not the ones the caller guessed.
    expect(f.taskRunner.enqueued).toEqual([
      {
        taskId: "task-order",
        scopeId: "scope-1",
        priority: 3,
        availableAt: f.clock.nowIso(),
      },
    ]);
  });

  it("treats a duplicate caller-supplied taskId as a redelivery: same task, one more poke", async () => {
    const f = setup();
    const input = {
      taskId: "task-dup",
      kind: "demo.echo",
      scopeId: "scope-1",
      payload: { attempt: "first" },
    };
    const first = await f.service.submitTask(input);
    const second = await f.service.submitTask({
      ...input,
      payload: { attempt: "second" },
    });

    expect(second.taskId).toBe(first.taskId);
    // The FIRST payload is what stands — a resubmit must not rewrite work that
    // may already be executing.
    expect(second.payload).toEqual({ attempt: "first" });
    // Dispatched again (the port is idempotent, so the poke is free) but the
    // queue only ever heard about one task.
    expect(f.enqueueCalls()).toBe(2);
    expect(f.taskRunner.enqueued).toHaveLength(1);
    expect(f.store.tasks.tasks.size).toBe(1);
  });

  it("rethrows when the duplicate id belongs to a DIFFERENT kind", async () => {
    const f = setup();
    await f.service.submitTask({
      taskId: "task-clash",
      kind: "demo.echo",
      scopeId: "scope-1",
      payload: {},
    });
    await expect(
      f.service.submitTask({
        taskId: "task-clash",
        kind: "index.rebuild",
        scopeId: "scope-1",
        payload: {},
      }),
    ).rejects.toThrow(DuplicateTaskError);
    // No second dispatch for a submit that was rejected.
    expect(f.enqueueCalls()).toBe(1);
  });

  it("rethrows a duplicate on a MINTED id — that is a broken IdGenerator, not a redelivery", async () => {
    const f = setup();
    // Pre-seed the id the generator is about to hand out.
    await f.store.tasks.createTask({
      taskId: "task-1",
      kind: "demo.echo",
      scopeId: "scope-1",
      payload: {},
    });
    await expect(
      f.service.submitTask({
        kind: "demo.echo",
        scopeId: "scope-1",
        payload: {},
      }),
    ).rejects.toThrow(DuplicateTaskError);
    expect(f.enqueueCalls()).toBe(0);
  });
});
