import { describe, expect, it } from "bun:test";
import { CONTRACT_VERSION } from "@agentkit/contracts";
import { LeaseLostError, createTaskEventWriter } from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/** A `running` task with an attempt and a live lease — an executor's world. */
async function seedAttempt(
  f: TestHarness,
  taskId: string,
): Promise<{ attemptId: string; leaseToken: string }> {
  await f.store.tasks.createTask({
    taskId,
    kind: "demo.echo",
    scopeId: "scope-1",
    payload: {},
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

describe("createTaskEventWriter", () => {
  it("stamps the base fields and appends under the lease", async () => {
    const f = createHarness();
    const seeded = await seedAttempt(f, "task-1");
    const writer = createTaskEventWriter({
      tasks: f.store.tasks,
      taskId: "task-1",
      ...seeded,
      clock: f.clock,
      ids: f.ids,
    });

    const event = await writer.emit({
      type: "demo.started",
      data: { note: "hello" },
    });

    expect(event.seq).toBe(0);
    expect(event.eventId).toBe("evt-1");
    expect(event.timestamp).toBe(f.clock.nowIso());
    expect(event.contractVersion).toBe(CONTRACT_VERSION);
    expect(event.attemptId).toBe(seeded.attemptId);
    // The vocabulary's own fields ride through untouched.
    expect((event as unknown as { data: unknown }).data).toEqual({
      note: "hello",
    });

    const stored = await f.store.tasks.listEvents("task-1");
    expect(stored).toEqual([event]);
  });

  it("continues the seq of an existing log rather than restarting it", async () => {
    const f = createHarness();
    const seeded = await seedAttempt(f, "task-1");
    // Two events already on the log, as an earlier pass (or attempt) left them.
    await f.store.tasks.appendEvents(
      "task-1",
      [
        {
          type: "demo.seeded",
          seq: 0,
          eventId: "evt-seed-0",
          timestamp: f.clock.nowIso(),
          contractVersion: CONTRACT_VERSION,
        },
        {
          type: "demo.seeded",
          seq: 1,
          eventId: "evt-seed-1",
          timestamp: f.clock.nowIso(),
          contractVersion: CONTRACT_VERSION,
        },
      ],
      { leaseToken: seeded.leaseToken },
    );

    const writer = createTaskEventWriter({
      tasks: f.store.tasks,
      taskId: "task-1",
      ...seeded,
      clock: f.clock,
      ids: f.ids,
    });
    const first = await writer.emit({ type: "demo.progress" });
    const second = await writer.emit({ type: "demo.completed" });

    expect(first.seq).toBe(2);
    expect(second.seq).toBe(3);
    const stored = await f.store.tasks.listEvents("task-1");
    expect(stored.map((e) => e.seq)).toEqual([0, 1, 2, 3]);
  });

  it("never lets a draft overwrite the base fields the writer owns", async () => {
    const f = createHarness();
    const seeded = await seedAttempt(f, "task-1");
    const writer = createTaskEventWriter({
      tasks: f.store.tasks,
      taskId: "task-1",
      ...seeded,
      clock: f.clock,
      ids: f.ids,
    });

    const event = await writer.emit({
      type: "demo.started",
      seq: 99,
      eventId: "forged",
      attemptId: "someone-elses-attempt",
    });

    // A draft that numbered itself would put a gap in the one sequence a
    // consumer uses to detect dropped events.
    expect(event.seq).toBe(0);
    expect(event.eventId).toBe("evt-1");
    expect(event.attemptId).toBe(seeded.attemptId);
  });

  it("propagates LeaseLostError when the attempt no longer owns the task", async () => {
    const f = createHarness();
    const seeded = await seedAttempt(f, "task-1");
    const writer = createTaskEventWriter({
      tasks: f.store.tasks,
      taskId: "task-1",
      ...seeded,
      clock: f.clock,
      ids: f.ids,
    });
    // A takeover: a fresh lease replaces the one this writer holds.
    await f.store.tasks.acquireLease({
      taskId: "task-1",
      attemptId: "att-takeover",
      ownerId: "worker-2",
      ttlMs: 60_000,
    });

    await expect(writer.emit({ type: "demo.progress" })).rejects.toThrow(
      LeaseLostError,
    );
    expect(await f.store.tasks.listEvents("task-1")).toEqual([]);
  });
});
