/**
 * The delay between attempts of one task.
 *
 * Before this existed, a transient failure started a fresh attempt on the very
 * next poll cycle — so a provider that was briefly down got three requests in
 * ~200ms and the task burned its whole attempt budget before the outage had a
 * chance to end. The tests below are all about WHEN the next attempt starts,
 * never about how long the suite sleeps: the runner measures the deadline
 * against its injected clock, so every assertion here is "advance the clock and
 * see", and the only real-time waits are 1ms condition polls.
 */
import { afterEach, describe, expect, it } from "bun:test";
import {
  createHarness,
  settle,
  waitFor,
  type Harness,
} from "./support/task-runner-harness.js";

const started: Array<{ harness: Harness; stop: () => Promise<void> }> = [];

async function start(harness: Harness, concurrency = 2): Promise<void> {
  const handle = await harness.runner.startWorker(harness.worker, {
    concurrency,
    ownerId: "owner-backoff",
  });
  started.push({ harness, stop: handle.stop });
}

afterEach(async () => {
  for (const entry of started.splice(0)) {
    entry.harness.worker.releaseAll();
    await entry.stop();
  }
});

/** An `ECONNRESET`-shaped failure: the classifier's transient evidence. */
function networkError(): Error {
  return Object.assign(new Error("connect ECONNRESET 10.0.0.4:443"), {
    code: "ECONNRESET",
  });
}

const attempts = (harness: Harness, taskId: string) =>
  harness.worker.callsFor(taskId).length;

describe("SingleProcessTaskRunner — retry backoff", () => {
  it("holds the next attempt until the deadline, then runs it", async () => {
    const harness = createHarness({
      maxAttempts: 3,
      retryBackoff: { baseMs: 1_000, jitterRatio: 0 },
    });
    const startedAt = harness.clock.now().getTime();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "complete" },
    ]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(() => attempts(harness, "run-1") === 1, "the first attempt");
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "the retry deadline to be armed",
    );
    expect(harness.runner.retryDeadline("run-1")).toBe(startedAt + 1_000);

    // The clock has not moved: many poll cycles pass and NOTHING starts.
    await settle();
    expect(attempts(harness, "run-1")).toBe(1);
    harness.clock.advance(999);
    await settle();
    expect(attempts(harness, "run-1")).toBe(1);

    harness.clock.advance(1);
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "completed",
      "run-1 to complete once the deadline passed",
    );
    expect(attempts(harness, "run-1")).toBe(2);
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "failed", attemptNumber: 1 },
      { status: "completed", attemptNumber: 2 },
    ]);
    // The delay changed nothing about the shape of a retry: same task, same
    // unbroken event sequence, no trip back through `queued`.
    const run = await harness.store.tasks.getTask("run-1");
    expect(run?.attemptCount).toBe(2);
    expect(run?.deadLetteredAt).toBeUndefined();
  });

  it("doubles the delay per attempt and caps it at maxMs", async () => {
    const harness = createHarness({
      maxAttempts: 4,
      retryBackoff: { baseMs: 1_000, maxMs: 1_500, jitterRatio: 0 },
    });
    const startedAt = harness.clock.now().getTime();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "throw", error: networkError() },
      { kind: "throw", error: networkError() },
      { kind: "complete" },
    ]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });

    // 1st failure: baseMs × 2^0.
    await waitFor(
      () => harness.runner.retryDeadline("run-1") === startedAt + 1_000,
      "a 1000ms deadline after the first failure",
    );
    harness.clock.advance(1_000);

    // 2nd failure: baseMs × 2^1 = 2000, capped by maxMs to 1500.
    await waitFor(
      () => harness.runner.retryDeadline("run-1") === startedAt + 1_000 + 1_500,
      "a maxMs-capped deadline after the second failure",
    );
    await settle();
    expect(attempts(harness, "run-1")).toBe(2);
    harness.clock.advance(1_500);

    // 3rd failure: 4000, still capped at 1500.
    await waitFor(
      () =>
        harness.runner.retryDeadline("run-1") ===
        startedAt + 1_000 + 1_500 + 1_500,
      "a maxMs-capped deadline after the third failure",
    );
    harness.clock.advance(1_500);

    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "completed",
      "run-1 to complete on its fourth attempt",
    );
    expect(attempts(harness, "run-1")).toBe(4);
  });

  it("spreads the delay within ± jitterRatio of the exact backoff", async () => {
    const harness = createHarness({
      maxAttempts: 3,
      retryBackoff: { baseMs: 1_000, jitterRatio: 0.2 },
    });
    const startedAt = harness.clock.now().getTime();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "complete" },
    ]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "the retry deadline to be armed",
    );
    const deadline = harness.runner.retryDeadline("run-1")!;
    expect(deadline).toBeGreaterThanOrEqual(startedAt + 800);
    expect(deadline).toBeLessThanOrEqual(startedAt + 1_200);

    harness.clock.advance(1_200);
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "completed",
      "run-1 to complete once the jittered deadline passed",
    );
  });

  it("keeps holding its scope while it backs off", async () => {
    // The invariant the in-place retry rests on: a task that started stays
    // `running` for its whole life, and nothing else in its scope may run
    // alongside it. A backoff is dead time in the middle of that life — if it
    // released the scope, a second task in the same chat would start while the
    // first is still on its way to a terminal state.
    const harness = createHarness({
      maxAttempts: 3,
      retryBackoff: { baseMs: 1_000, jitterRatio: 0 },
    });
    await harness.seedTask("run-1", "chat-1");
    await harness.seedTask("run-2", "chat-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "complete" },
    ]);
    harness.worker.script("run-2", [{ kind: "complete" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await harness.runner.enqueue({ taskId: "run-2", scopeId: "chat-1" });
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "run-1 to be backing off",
    );
    await settle();

    expect(attempts(harness, "run-2")).toBe(0);
    expect((await harness.store.tasks.getTask("run-2"))?.status).toBe("queued");

    harness.clock.advance(1_000);
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-2"))?.status === "completed",
      "run-2 to run after run-1 finished",
    );
    // Strict ordering: run-1's whole life, backoff included, precedes run-2.
    expect(harness.worker.timeline.indexOf("start:run-2")).toBeGreaterThan(
      harness.worker.timeline.lastIndexOf("end:run-1"),
    );
  });

  it("skips the remaining backoff when a cancel lands mid-wait", async () => {
    const harness = createHarness({
      maxAttempts: 3,
      retryBackoff: { baseMs: 60_000, jitterRatio: 0 },
    });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "await-abort" },
    ]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "run-1 to be backing off",
    );

    // A minute of backoff on a frozen clock: only the cancel can end this.
    await harness.runner.requestCancel("run-1");
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "cancelled",
      "run-1 to land cancelled without waiting out the backoff",
    );
    expect(attempts(harness, "run-1")).toBe(2);
  });

  it("defers to recovery when the worker stops mid-backoff", async () => {
    const harness = createHarness({
      maxAttempts: 3,
      retryBackoff: { baseMs: 60_000, jitterRatio: 0 },
      leaseTtlMs: 1_000,
    });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "complete" },
    ]);
    const handle = await harness.runner.startWorker(harness.worker, {
      concurrency: 1,
      ownerId: "owner-backoff",
    });

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "run-1 to be backing off",
    );

    // `stop()` must not wait out the backoff — a 60s delay on a frozen clock
    // would hang shutdown outright — and must not start an attempt for a worker
    // that is gone. The task is left exactly where the pre-backoff
    // "retry deferred to recovery" branch leaves it: `running`, one failed
    // attempt, nothing dispatched.
    await handle.stop();
    expect(attempts(harness, "run-1")).toBe(1);
    expect((await harness.store.tasks.getTask("run-1"))?.status).toBe(
      "running",
    );
    expect(harness.runner.retryDeadline("run-1")).toBeUndefined();
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "failed", attemptNumber: 1 },
    ]);
  });
});
