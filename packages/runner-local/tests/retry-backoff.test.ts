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
  createSecondRunner,
  settle,
  waitFor,
  type FakeWorker,
  type Harness,
} from "./support/task-runner-harness.js";

/**
 * Every runner started by a test, with the worker whose held executions have to
 * be released before it can stop. A test that recovers a task with a SECOND
 * runner registers that one here too, so a failed assertion cannot leave two
 * claim loops running.
 */
const started: Array<{ release: () => void; stop: () => Promise<void> }> = [];

function track(worker: FakeWorker, stop: () => Promise<void>): void {
  started.push({ release: () => worker.releaseAll(), stop });
}

async function start(
  harness: Harness,
  concurrency = 2,
): Promise<{ stop: () => Promise<void> }> {
  const handle = await harness.runner.startWorker(harness.worker, {
    concurrency,
    ownerId: "owner-backoff",
  });
  track(harness.worker, handle.stop);
  return handle;
}

afterEach(async () => {
  for (const entry of started.splice(0)) {
    entry.release();
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

  it("spreads the delay within ± jitterRatio, and does not spread it by the same amount every time", async () => {
    // One sample cannot tell jitter from no jitter: the exact backoff sits
    // INSIDE the band a single sample is checked against, so a runner that
    // ignored `jitterRatio` entirely would pass that assertion forever. What
    // jitter is FOR is N tasks that failed on the same outage not retrying in
    // lockstep — so the test that kills "jitter ignored" has to sample several
    // delays and find them different.
    //
    // `maxMs === baseMs` pins the exact, un-jittered delay at 1000ms for EVERY
    // attempt (the doubling is capped away), which is what makes the samples
    // comparable to each other and to a constant.
    const EXACT_MS = 1_000;
    const SAMPLES = 5;
    const harness = createHarness({
      maxAttempts: 50,
      retryBackoff: { baseMs: EXACT_MS, maxMs: EXACT_MS, jitterRatio: 0.2 },
    });
    await harness.seedTask("run-1");
    // The last entry repeats: every attempt fails, so every one backs off.
    harness.worker.script("run-1", [{ kind: "throw", error: networkError() }]);
    await start(harness, 1);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });

    const offsets: number[] = [];
    let previous: number | undefined;
    for (let sample = 0; sample < SAMPLES; sample += 1) {
      await waitFor(
        () => {
          const deadline = harness.runner.retryDeadline("run-1");
          return deadline !== undefined && deadline !== previous;
        },
        `backoff ${sample + 1} to be armed`,
      );
      const deadline = harness.runner.retryDeadline("run-1")!;
      previous = deadline;
      // The clock only moves when this test moves it, so the remaining time IS
      // the delay the runner chose.
      offsets.push(deadline - harness.clock.now().getTime() - EXACT_MS);
      // Past any jittered 1000 ± 200, so the next attempt starts and fails.
      harness.clock.advance(EXACT_MS * 2);
    }

    // Inside the band...
    for (const offset of offsets) {
      expect(Math.abs(offset)).toBeLessThanOrEqual(EXACT_MS * 0.2);
    }
    // ...and actually spread. `jitterRatio` ignored ⇒ every offset is exactly 0.
    expect(new Set(offsets).size).toBeGreaterThan(1);
  });

  it("uses the exact backoff when jitterRatio is 0", async () => {
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
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "the retry deadline to be armed",
    );
    expect(harness.runner.retryDeadline("run-1")).toBe(startedAt + 1_000);

    harness.clock.advance(1_000);
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "completed",
      "run-1 to complete once the deadline passed",
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

    // AND THE LEASE IS STILL THERE. "Deferred to recovery" is only true if
    // something can still find the task, and a lease is the only thing that
    // can: `running` is not claimable and `recover()` reads expired leases, not
    // task rows. Renewing the stopped attempt's own token is the port-level
    // proof the lease survived the shutdown — a released one is gone, and
    // `renewLease` answers `LeaseLostError` for a token that is not current.
    const staleToken = harness.worker.callsFor("run-1")[0]!.leaseToken;
    const renewed = await harness.store.tasks.renewLease(staleToken, 1_000);
    expect(renewed.taskId).toBe("run-1");
  });

  it("hands the task a NEXT PROCESS can recover after stopping mid-backoff", async () => {
    // The previous test proves the lease survives; this one proves that is
    // enough. A second runner over the same store — the process that starts
    // after this one exits — must be able to pick the task up and finish it,
    // which is the entire promise the "deferred to recovery" branch makes.
    const harness = createHarness({
      maxAttempts: 3,
      retryBackoff: { baseMs: 60_000, jitterRatio: 0 },
      leaseTtlMs: 1_000,
    });
    await harness.seedTask("run-1");
    // Only ONE scripted behaviour: this worker must never run run-1 again.
    harness.worker.script("run-1", [{ kind: "throw", error: networkError() }]);
    const handle = await harness.runner.startWorker(harness.worker, {
      concurrency: 1,
      ownerId: "owner-crashed",
    });

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "run-1 to be backing off",
    );
    await handle.stop();

    // The lease outlives the process by design, then expires on its own.
    harness.clock.advance(1_500);

    const next = createSecondRunner(harness, {
      leaseTtlMs: 1_000,
      maxAttempts: 3,
    });
    next.worker.script("run-1", [{ kind: "complete" }]);
    const report = await next.runner.recoverWithReport();
    expect(report.expired).toBe(1);
    const nextHandle = await next.runner.startWorker(next.worker, {
      concurrency: 1,
      ownerId: "owner-next",
    });
    track(next.worker, nextHandle.stop);

    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "completed",
      "the next process to finish the interrupted task",
    );
    expect(next.worker.callsFor("run-1").length).toBe(1);
    // The stopped runner wrote nothing more: one attempt, one execution, and no
    // third attempt row from a retry it decided to start after all.
    expect(attempts(harness, "run-1")).toBe(1);
    await settle();
    expect(harness.attemptsFor("run-1")).toEqual([
      // Attempt 1 ended `failed` when the worker threw, then `abandoned` when
      // the recovery pass found its lease expired — the last word is honest:
      // nobody knows whether that attempt's work landed.
      { status: "abandoned", attemptNumber: 1 },
      { status: "completed", attemptNumber: 2 },
    ]);
  });

  it("does not steal the task back when the lease moved during the backoff", async () => {
    // A backoff can outlast the lease TTL, and an expired lease is exactly what
    // another owner's `recover()` acts on. Without a fencing check AFTER the
    // wait, this runner wakes up, mints a fresh lease and runs a task the
    // recovery already handed to somebody else — two executions of one task, at
    // the same time, on one event stream.
    const harness = createHarness({
      maxAttempts: 5,
      retryBackoff: { baseMs: 10_000, jitterRatio: 0 },
      leaseTtlMs: 1_000,
    });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      // Must never be reached: reaching it IS the bug.
      { kind: "complete" },
    ]);
    await start(harness, 1);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.runner.retryDeadline("run-1") !== undefined,
      "run-1 to be backing off",
    );

    // The lease expires mid-wait (a heartbeat that could not reach the store, a
    // process that was paged out) and a second owner recovers the task.
    harness.clock.advance(1_500);
    const next = createSecondRunner(harness, {
      leaseTtlMs: 1_000,
      maxAttempts: 5,
    });
    next.worker.script("run-1", [{ kind: "hold" }]);
    const nextHandle = await next.runner.startWorker(next.worker, {
      concurrency: 1,
      ownerId: "owner-recovered",
    });
    track(next.worker, nextHandle.stop);
    await next.runner.recover();
    await waitFor(
      () => next.worker.callsFor("run-1").length === 1,
      "the recovered attempt to start",
    );

    // Now let the ORIGINAL backoff finish, with the recovered attempt still
    // running. Nothing may start here.
    harness.clock.advance(10_000);
    await settle(100);
    expect(attempts(harness, "run-1")).toBe(1);
    expect(next.worker.callsFor("run-1").length).toBe(1);
    expect(harness.attemptsFor("run-1").length).toBe(2);

    next.worker.release("run-1");
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("run-1"))?.status === "completed",
      "the recovered attempt to complete",
    );
    await settle();
    // Two executions in the task's whole life, never overlapping: the failed
    // one, and the recovered one that finished it.
    expect(attempts(harness, "run-1")).toBe(1);
    expect(next.worker.callsFor("run-1").length).toBe(1);
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "abandoned", attemptNumber: 1 },
      { status: "completed", attemptNumber: 2 },
    ]);
  });
});
