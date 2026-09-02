/**
 * Behavioural suite for {@link SingleProcessTaskRunner}: dispatch, scope
 * serialization, the retry taxonomy, cancellation, and crash recovery with
 * fencing.
 *
 * Every timing assertion is driven by an injected clock; the only real-time
 * waits are 1ms condition polls, so the suite runs in milliseconds and does not
 * depend on how fast the machine is.
 */
import { afterEach, describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { LeaseLostError } from "@agentkit/host";
import { createTestEventStamper } from "@agentkit/testing";
import {
  createHarness,
  settle,
  waitFor,
  type Harness,
} from "./support/task-runner-harness.js";

/** Every runner started by a test, so a failure cannot leave a loop running. */
const started: Array<{ harness: Harness; stop: () => Promise<void> }> = [];

async function start(
  harness: Harness,
  concurrency = 2,
): Promise<{ stop: () => Promise<void> }> {
  const handle = await harness.runner.startWorker(harness.worker, {
    concurrency,
    ownerId: "owner-1",
  });
  started.push({ harness, stop: handle.stop });
  return handle;
}

afterEach(async () => {
  for (const entry of started.splice(0)) {
    // Unblock anything still held FIRST: `stop()` waits for in-flight work by
    // contract, so a failed assertion mid-hold would otherwise hang the suite
    // instead of reporting the failure.
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

const taskStatus = async (harness: Harness, taskId: string) =>
  (await harness.store.tasks.getTask(taskId))?.status;

describe("SingleProcessTaskRunner — dispatch", () => {
  it("(a) executes an enqueued run once and leaves it completed", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "complete" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "run-1 to complete",
    );

    expect(harness.worker.callsFor("run-1").length).toBe(1);
    // The worker finalized the run itself (as TurnRunner does); the runner
    // recognised that and wrote nothing on top of it.
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "completed", attemptNumber: 1 },
    ]);
    const events = (await harness.store.tasks.listEvents(
      "run-1",
    )) as AiRunEvent[];
    expect(events.map((event) => event.seq)).toEqual([0]);
    const run = await harness.store.tasks.getTask("run-1");
    expect(run?.finishedAt).toBeDefined();
    expect(run?.deadLetteredAt).toBeUndefined();
  });

  it("(b) is idempotent: re-enqueuing a run that is not queued does nothing", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "hold" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to start",
    );

    // Re-delivery while it is running, and again after it finished.
    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    harness.worker.release("run-1");
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "run-1 to complete",
    );
    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await settle();

    expect(harness.worker.callsFor("run-1").length).toBe(1);
    expect(harness.attemptsFor("run-1").length).toBe(1);
  });

  it("rejects an enqueue for a run that was never persisted", async () => {
    const harness = createHarness();
    let code: string | undefined;
    try {
      await harness.runner.enqueue({ taskId: "ghost", scopeId: "chat-1" });
    } catch (err) {
      code = (err as { code?: string }).code;
    }
    expect(code).toBe("not_found");
  });

  it("(c) runs two scopes at once — dispatch is fire-and-forget", async () => {
    const harness = createHarness();
    await harness.seedTask("run-a", "chat-a");
    await harness.seedTask("run-b", "chat-b");
    harness.worker.script("run-a", [{ kind: "hold" }]);
    harness.worker.script("run-b", [{ kind: "hold" }]);
    await start(harness, 2);

    await harness.runner.enqueue({ taskId: "run-a", scopeId: "chat-a" });
    await harness.runner.enqueue({ taskId: "run-b", scopeId: "chat-b" });
    await waitFor(
      () => harness.worker.calls.length === 2,
      "both runs to be executing",
    );

    // The overlap is the assertion: a runner that awaited its dispatch would
    // never reach two concurrent executions (the task-system bug).
    expect(harness.worker.peakConcurrency).toBe(2);
    expect(harness.worker.timeline.filter((e) => e.startsWith("end:"))).toEqual(
      [],
    );

    harness.worker.release("run-a");
    harness.worker.release("run-b");
    await waitFor(
      async () =>
        (await taskStatus(harness, "run-a")) === "completed" &&
        (await taskStatus(harness, "run-b")) === "completed",
      "both runs to complete",
    );
  });

  it("(d) serializes two runs sharing a scope, and reports the queue position", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1", "chat-1");
    await harness.seedTask("run-2", "chat-1");
    harness.worker.script("run-1", [{ kind: "hold" }]);
    harness.worker.script("run-2", [{ kind: "complete" }]);
    await start(harness, 2);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to start",
    );
    await harness.runner.enqueue({ taskId: "run-2", scopeId: "chat-1" });

    expect(harness.runner.scopeLock.getPosition("chat-1", "run-1")).toBe(0);
    expect(harness.runner.scopeLock.getPosition("chat-1", "run-2")).toBe(1);
    await settle();
    expect(harness.worker.callsFor("run-2").length).toBe(0);
    expect(await taskStatus(harness, "run-2")).toBe("queued");

    harness.worker.release("run-1");
    await waitFor(
      async () => (await taskStatus(harness, "run-2")) === "completed",
      "run-2 to complete",
    );
    expect(harness.worker.timeline).toEqual([
      "start:run-1",
      "end:run-1",
      "start:run-2",
      "end:run-2",
    ]);
    expect(harness.runner.scopeLock.getPosition("chat-1", "run-2")).toBe(-1);
  });
});

describe("SingleProcessTaskRunner — failure taxonomy", () => {
  it("(e) retries a transient failure in place, on one run and one event stream", async () => {
    const harness = createHarness({ maxAttempts: 3 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: networkError() },
      { kind: "complete" },
    ]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "run-1 to complete after a retry",
    );

    expect(harness.worker.callsFor("run-1").length).toBe(2);
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "failed", attemptNumber: 1 },
      { status: "completed", attemptNumber: 2 },
    ]);
    // One run, one unbroken sequence: the retry never went back to `queued`
    // and never restarted the log.
    const events = (await harness.store.tasks.listEvents(
      "run-1",
    )) as AiRunEvent[];
    expect(events.map((event) => event.seq)).toEqual([0, 1]);
    const run = await harness.store.tasks.getTask("run-1");
    expect(run?.attemptCount).toBe(2);
    expect(run?.deadLetteredAt).toBeUndefined();
    // Each attempt wrote under its own lease token.
    const [first, second] = harness.worker.callsFor("run-1");
    expect(first!.leaseToken).not.toBe(second!.leaseToken);
  });

  it("(f) fails a terminal error immediately, with no retry and no dead-letter", async () => {
    const harness = createHarness({ maxAttempts: 3 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      { kind: "throw", error: new Error("401 unauthorized: invalid api key") },
    ]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "failed",
      "run-1 to fail",
    );
    await settle();

    expect(harness.worker.callsFor("run-1").length).toBe(1);
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "failed", attemptNumber: 1 },
    ]);
    const run = await harness.store.tasks.getTask("run-1");
    // Dead-letter is for poison retry loops, not for a diagnosed failure.
    expect(run?.deadLetteredAt).toBeUndefined();
    expect(run?.error).toContain("http_rejected");
  });

  it("(g) dead-letters a run whose transient failures exhaust the budget", async () => {
    const harness = createHarness({ maxAttempts: 2 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "throw", error: networkError() }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "failed",
      "run-1 to exhaust its attempts",
    );
    await settle();

    expect(harness.worker.callsFor("run-1").length).toBe(2);
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "failed", attemptNumber: 1 },
      { status: "failed", attemptNumber: 2 },
    ]);
    const run = await harness.store.tasks.getTask("run-1");
    expect(run?.deadLetteredAt).toBeDefined();
    expect(run?.deadLetterReason).toContain("network:ECONNRESET");
    // Nothing re-dispatches a dead-lettered run: it is not `queued`.
    expect(harness.worker.callsFor("run-1").length).toBe(2);
  });
});

describe("SingleProcessTaskRunner — cancellation", () => {
  it("(h) cancels a queued run before any worker sees it", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1");
    await start(harness);

    await harness.runner.requestCancel("run-1");
    expect(await taskStatus(harness, "run-1")).toBe("cancelled");

    await settle();
    expect(harness.worker.calls.length).toBe(0);
    expect(harness.attemptsFor("run-1")).toEqual([]);
    expect(harness.runner.scopeLock.busyScopes()).toEqual([]);
  });

  it("(i) aborts a running run's signal and tolerates the worker landing it", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "await-abort" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to start",
    );
    await harness.runner.requestCancel("run-1");

    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "cancelled",
      "run-1 to land cancelled",
    );
    await settle();

    // The worker finalized; the runner must not have transitioned again (the
    // store would have thrown) nor overwritten the attempt's outcome.
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "cancelled", attemptNumber: 1 },
    ]);
    const events = (await harness.store.tasks.listEvents(
      "run-1",
    )) as AiRunEvent[];
    expect(events.map((event) => event.type)).toEqual(["run.cancelled"]);
    expect(harness.worker.callsFor("run-1").length).toBe(1);
  });

  it("ignores a cancel for a run that already finished", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "complete" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "run-1 to complete",
    );
    await harness.runner.requestCancel("run-1");
    expect(await taskStatus(harness, "run-1")).toBe("completed");
  });
});

describe("SingleProcessTaskRunner — recovery", () => {
  it("(j) re-dispatches an abandoned run and fences the dead attempt's lease", async () => {
    // Tiny lease TTL on the fake clock; the heartbeat interval is far longer
    // than the test's real lifetime, so nothing renews behind our back.
    const harness = createHarness({ leaseTtlMs: 1_000, maxAttempts: 3 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "never" }, { kind: "hold" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to start",
    );
    const staleToken = harness.worker.callsFor("run-1")[0]!.leaseToken;

    // The owner "died": nothing renews, and the clock walks past the expiry.
    harness.clock.advance(1_500);
    const report = await harness.runner.recoverWithReport();
    expect(report).toEqual({ expired: 1, redispatched: 1, deadLettered: 0 });

    await waitFor(
      () => harness.worker.callsFor("run-1").length === 2,
      "the recovered attempt to start",
    );
    const freshToken = harness.worker.callsFor("run-1")[1]!.leaseToken;
    expect(freshToken).not.toBe(staleToken);

    // THE FENCING ACCEPTANCE TEST: the dead attempt cannot write any more, the
    // live one can — even though both believe they are executing run-1.
    const stamp = createTestEventStamper({ firstSeq: 0, attemptId: "att-x" });
    const event = stamp({
      type: "run.warning",
      runId: "run-1",
      timestamp: harness.clock.nowIso(),
      data: { code: "probe", message: "from the zombie" },
    });
    let staleCode: unknown;
    try {
      await harness.store.tasks.appendEvents("run-1", [event], {
        leaseToken: staleToken,
      });
    } catch (err) {
      staleCode = err;
    }
    expect(staleCode).toBeInstanceOf(LeaseLostError);
    await harness.store.tasks.appendEvents("run-1", [event], {
      leaseToken: freshToken,
    });

    harness.worker.release("run-1");
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "the recovered attempt to complete",
    );
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "abandoned", attemptNumber: 1 },
      { status: "completed", attemptNumber: 2 },
    ]);
    // One run, one event stream: the recovered attempt continued the sequence.
    const events = (await harness.store.tasks.listEvents(
      "run-1",
    )) as AiRunEvent[];
    expect(events.map((e) => e.seq)).toEqual([0, 1]);

    harness.worker.releaseAll();
  });

  it("(k) dead-letters an abandoned run that has no attempts left", async () => {
    const harness = createHarness({ leaseTtlMs: 1_000, maxAttempts: 1 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "never" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to start",
    );

    harness.clock.advance(1_500);
    const report = await harness.runner.recoverWithReport();
    expect(report).toEqual({ expired: 1, redispatched: 0, deadLettered: 1 });

    const run = await harness.store.tasks.getTask("run-1");
    expect(run?.status).toBe("failed");
    expect(run?.deadLetteredAt).toBeDefined();
    expect(run?.deadLetterReason).toContain("poison");
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "abandoned", attemptNumber: 1 },
    ]);
    expect(harness.worker.callsFor("run-1").length).toBe(1);

    harness.worker.releaseAll();
  });

  it("re-runs a task recovered BEFORE the worker started — the documented boot order", async () => {
    // `recoverOnBoot(...)` then `startWorker(...)` is the order every host
    // wires (examples/desktop-host/src/wiring.ts, steps 9 and 10). The expiry
    // pass DELETES the lease and ends the attempt `abandoned` before it
    // discovers there is no worker yet — so unless the runner remembers the
    // task, it is left `running` with no lease: invisible to the claim loop
    // (which only takes `queued`) and to every later `recover()` (which only
    // sees expired leases). Nothing would ever run it again.
    const harness = createHarness({ leaseTtlMs: 1_000, maxAttempts: 3 });
    await harness.seedTask("run-1");
    // What a process that died mid-attempt leaves behind: a `running` task and
    // a lease nobody is renewing.
    const attempt = await harness.store.tasks.createAttempt({
      attemptId: "att-crashed",
      taskId: "run-1",
      ownerId: "dead-owner",
    });
    await harness.store.tasks.transitionTask("run-1", ["queued"], "running", {
      startedAt: harness.clock.nowIso(),
    });
    await harness.store.tasks.acquireLease({
      taskId: "run-1",
      attemptId: attempt.attemptId,
      ownerId: "dead-owner",
      ttlMs: 1_000,
    });
    harness.worker.script("run-1", [{ kind: "complete" }]);
    harness.clock.advance(1_500);

    // Step 9: recover, with nothing yet to hand the work to.
    const report = await harness.runner.recoverWithReport();
    expect(report).toEqual({ expired: 1, redispatched: 0, deadLettered: 0 });
    expect(await taskStatus(harness, "run-1")).toBe("running");

    // Step 10: start claiming. The interrupted task goes out immediately.
    await start(harness);
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "the interrupted task to be re-executed once the worker started",
    );
    expect(harness.worker.callsFor("run-1").length).toBe(1);
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "abandoned", attemptNumber: 1 },
      { status: "completed", attemptNumber: 2 },
    ]);
  });

  it("leaves an abandoned run for the next owner when no worker is running", async () => {
    const harness = createHarness({ leaseTtlMs: 1_000, maxAttempts: 3 });
    await harness.seedTask("run-1");
    // A lease acquired by nobody in particular — the crashed-process shape.
    const attempt = await harness.store.tasks.createAttempt({
      attemptId: "att-orphan",
      taskId: "run-1",
      ownerId: "dead-owner",
    });
    await harness.store.tasks.transitionTask("run-1", ["queued"], "running", {
      startedAt: harness.clock.nowIso(),
    });
    await harness.store.tasks.acquireLease({
      taskId: "run-1",
      attemptId: attempt.attemptId,
      ownerId: "dead-owner",
      ttlMs: 1_000,
    });

    harness.clock.advance(1_500);
    const report = await harness.runner.recoverWithReport();

    // Nothing is dispatched and nothing is invented: the attempt is recorded
    // `abandoned` and the task stays `running`. Where it goes NEXT is the
    // previous test's subject — this one is only about the pass itself being
    // safe to run with no worker.
    expect(report).toEqual({ expired: 1, redispatched: 0, deadLettered: 0 });
    expect(await taskStatus(harness, "run-1")).toBe("running");
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "abandoned", attemptNumber: 1 },
    ]);
  });
});

describe("SingleProcessTaskRunner — the runner's own terminal block", () => {
  it("finalizes a worker that returned without landing the task", async () => {
    // The defensive branch: leaving the task `running` forever with no lease
    // would make it invisible to both the claim loop and `recover()`.
    const harness = createHarness();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "quiet" }]);
    await start(harness, 1);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      async () => (await taskStatus(harness, "run-1")) === "completed",
      "the runner to land the task the worker left running",
    );
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "completed", attemptNumber: 1 },
    ]);
  });

  it("leaves the attempt RUNNING when the fenced transition throws", async () => {
    // The fence order is the whole point: task transition first, attempt
    // second. Ending the attempt first left a terminal attempt row under a
    // `running` task with a live lease whenever the transition failed — the
    // state `recover()` reads as a crash, so it would then end that same
    // already-terminal attempt `abandoned` and count a completion as poison.
    const harness = createHarness();
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "quiet" }]);

    const tasks = harness.store.tasks;
    const transitionTask = tasks.transitionTask.bind(tasks);
    tasks.transitionTask = async (taskId, from, to, patch, opts) => {
      if (taskId === "run-1" && to === "completed") {
        throw new Error("the store said no");
      }
      return transitionTask(taskId, from, to, patch, opts);
    };

    await start(harness, 1);
    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "the attempt to run",
    );
    await settle();

    // Nothing after the throw ran, so the task is exactly what recovery is
    // built to find: `running`, with a live lease and an attempt still open.
    expect(await taskStatus(harness, "run-1")).toBe("running");
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "running", attemptNumber: 1 },
    ]);
    const leaseToken = harness.worker.callsFor("run-1")[0]!.leaseToken;
    expect(
      (await harness.store.tasks.renewLease(leaseToken, 1_000)).taskId,
    ).toBe("run-1");
  });

  it("leaves the attempt RUNNING when the fenced CANCEL transition throws", async () => {
    // Same fence rule, on the throw path: `settleThrown`'s cancelled branch
    // ended the attempt before landing the task, so a transition that failed
    // left a `cancelled` attempt row under a `running` task with a live lease —
    // the state recovery reads as a crash.
    const harness = createHarness();
    await harness.seedTask("run-1");

    // A worker that throws on its way OUT of a cancel. The scripted
    // `await-abort` cannot reach this branch: it lands the task itself, which
    // is `settleResolved`'s business.
    let entered!: () => void;
    const running = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const tasks = harness.store.tasks;
    const transitionTask = tasks.transitionTask.bind(tasks);
    tasks.transitionTask = async (taskId, from, to, patch, opts) => {
      if (taskId === "run-1" && to === "cancelled") {
        throw new Error("the store said no");
      }
      return transitionTask(taskId, from, to, patch, opts);
    };

    const handle = await harness.runner.startWorker(
      {
        async execute({ signal }) {
          entered();
          await new Promise<void>((resolve) => {
            if (signal.aborted) resolve();
            else
              signal.addEventListener("abort", () => resolve(), {
                once: true,
              });
          });
          throw new Error("the worker threw on its way out of the cancel");
        },
      },
      { concurrency: 1, ownerId: "owner-1" },
    );
    started.push({ harness, stop: handle.stop });

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await running;
    await harness.runner.requestCancel("run-1");
    await settle();

    expect(await taskStatus(harness, "run-1")).toBe("running");
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "running", attemptNumber: 1 },
    ]);
  });

  it("leaves the attempt RUNNING when the fenced FAIL transition throws", async () => {
    // And on the terminal-error branch. A diagnosed failure lands the task
    // `failed`; the attempt row is closed only once that landed.
    const harness = createHarness({ maxAttempts: 3 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [
      // Terminal, not transient: a transient error would retry instead of
      // landing, and the retry path deliberately ends the attempt under a task
      // that stays `running`.
      { kind: "throw", error: new Error("401 unauthorized: invalid api key") },
    ]);

    const tasks = harness.store.tasks;
    const transitionTask = tasks.transitionTask.bind(tasks);
    tasks.transitionTask = async (taskId, from, to, patch, opts) => {
      if (taskId === "run-1" && to === "failed") {
        throw new Error("the store said no");
      }
      return transitionTask(taskId, from, to, patch, opts);
    };

    await start(harness, 1);
    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "the attempt to run",
    );
    await settle();

    expect(await taskStatus(harness, "run-1")).toBe("running");
    expect(harness.attemptsFor("run-1")).toEqual([
      { status: "running", attemptNumber: 1 },
    ]);
  });
});
