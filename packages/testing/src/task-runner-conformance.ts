// A shared behavioral contract every TaskRunner implementation must pass —
// the single-process reference runner, or anything else a host writes later
// (a Redis-backed queue, a cloud dispatcher).
//
// FRAMEWORK-NEUTRAL BY DESIGN, exactly like `store-conformance.ts`: this file
// must not import "bun:test" (or any other test runner). @agentkit/testing's
// build tsconfig sets `types: []` so nothing here can quietly depend on one
// runner's ambient globals, and `scripts/node-smoke.mjs` loads this package's
// dist under plain Node — a top-level `bun:test` import would break both. The
// test primitives (`describe`, `it`, `expect`) are INJECTED by the caller, and
// every @agentkit/host import is `import type`.
//
// WHAT THIS SUITE IS ABOUT, and what it deliberately leaves alone: the five
// promises the `TaskRunner` port makes that a store cannot make for it —
// enqueue idempotency, recovery from a dead owner, LEASE RENEWAL keeping a
// long attempt out of that recovery's way, cancellation reaching a running
// worker, and the concurrency budget actually meaning something.
// Everything below is observed THROUGH THE STORE (attempt rows, task status,
// dead-letter fields), never through an implementation's own bookkeeping, so a
// runner with a completely different internal design is still gradeable. Retry
// *policy* — which errors are transient, how long a backoff waits — is not in
// here: the port does not specify it, and two runners are allowed to disagree.
import type { AssistantStore, TaskExecution, TaskRunner } from "@agentkit/host";
import type {
  AssistantStoreConformanceTestApi,
  ConformanceClock,
} from "./conformance-support.js";

/** What `create()` hands back for one test: a fresh runner, its store, and the knobs the suite has to know. */
export interface TaskRunnerConformanceHarness {
  /** A runner with NO worker started yet — the suite calls `startWorker` itself. */
  runner: TaskRunner;
  /** The store the runner was built over; the suite reads its verdicts from here. */
  store: AssistantStore;
  /** Drives both the runner's and the store's sense of `now`. */
  clock: ConformanceClock;
  /** Attempts the runner allows per task before dead-lettering it. */
  maxAttempts: number;
  /** Lease lifetime, so the suite can advance the clock past it on purpose. */
  leaseTtlMs: number;
  /** Writes the `queued` task row a host would have written in its own transaction. */
  seedTask(input: { taskId: string; scopeId: string }): Promise<void>;
  /**
   * Attempt rows for a task, in any order — the suite sorts them.
   *
   * Supplied by the adapter because `TaskStore` has no `listAttempts`: attempt
   * history is written through the port but only ever read back by whoever owns
   * the storage. It is still the honest place to check "the abandoned attempt
   * was recorded as abandoned", so the suite asks for it rather than settling
   * for `attemptCount`.
   */
  attemptsFor(
    taskId: string,
  ): Promise<Array<{ attemptNumber: number; status: string }>>;
  /** Releases whatever `create()` opened (a db handle, a temp file). */
  close?: () => void;
}

/** Per-test knobs the suite hands `create()`. */
export interface TaskRunnerConformanceHarnessOptions {
  /**
   * Lease-renewal interval, in REAL milliseconds, that the runner must be built
   * with. MUST be honoured when present.
   *
   * Every other scenario wants renewal effectively off — they advance the clock
   * past the lease TTL on purpose and a heartbeat firing in the gap would
   * quietly rescue the lease they need expired — so `create()` should default to
   * an interval far longer than any test's real lifetime. The renewal scenario
   * is the one that asks for a short one.
   */
  heartbeatMs?: number;
}

export interface DescribeTaskRunnerConformanceOptions {
  /** Runner name, folded into the `describe` block title. */
  name: string;
  /** Builds one fresh, isolated runner+store per test — never shared across `it()`s. */
  create: (
    options?: TaskRunnerConformanceHarnessOptions,
  ) => Promise<TaskRunnerConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** How one attempt behaves when the worker is handed it. */
type Behavior =
  /** Land the task `completed` the way a real executor does, then return. */
  | "complete"
  /** Block until the test releases it, then behave like `complete`. */
  | "hold"
  /** Never resolve on its own, write nothing: the process that died mid-attempt. */
  | "never"
  /** Block until the signal aborts, then return WITHOUT landing the task. */
  | "await-abort";

interface ConformanceWorker {
  execute(execution: TaskExecution): Promise<void>;
  /** Every execute() call, in order. */
  readonly calls: TaskExecution[];
  /** Highest number of executions running at the same instant. */
  readonly peak: () => number;
  /** How many executions are running right now. */
  readonly running: () => number;
  callsFor(taskId: string): TaskExecution[];
  /** Which of `calls` saw its signal aborted. */
  abortedCount(): number;
  /** Unblock every held/never execution — teardown, and the end of a `hold`. */
  releaseAll(): void;
}

/**
 * A worker that writes through the store the way a real executor does
 * (transition, then end the attempt), so "the worker landed the task" is a
 * statement about the store rather than about a mock's opinion.
 */
function createWorker(
  store: AssistantStore,
  behaviorFor: (taskId: string, callIndex: number) => Behavior,
  nowIso: () => string,
): ConformanceWorker {
  const calls: TaskExecution[] = [];
  const gates: Array<() => void> = [];
  const aborted = new Set<string>();
  let running = 0;
  let peak = 0;

  const worker: ConformanceWorker = {
    calls,
    peak: () => peak,
    running: () => running,
    callsFor: (taskId) => calls.filter((call) => call.taskId === taskId),
    abortedCount: () => aborted.size,
    releaseAll: () => {
      for (const gate of gates.splice(0)) gate();
    },
    async execute(execution) {
      const index = calls.filter(
        (call) => call.taskId === execution.taskId,
      ).length;
      calls.push(execution);
      running += 1;
      peak = Math.max(peak, running);
      try {
        const behavior = behaviorFor(execution.taskId, index);
        if (behavior === "await-abort") {
          await untilAborted(execution.signal);
          aborted.add(execution.attemptId);
          return; // The runner decides how a cancelled attempt lands.
        }
        if (behavior === "hold" || behavior === "never") {
          await new Promise<void>((resolve) => gates.push(resolve));
          if (behavior === "never") return;
        }
        if (execution.signal.aborted) aborted.add(execution.attemptId);
        await store.tasks.transitionTask(
          execution.taskId,
          ["running"],
          "completed",
          { finishedAt: nowIso() },
        );
        await store.tasks.endAttempt({
          attemptId: execution.attemptId,
          status: "completed",
        });
      } finally {
        running -= 1;
      }
    },
  };
  return worker;
}

function untilAborted(signal: AbortSignal): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise<void>((resolve) => {
    signal.addEventListener("abort", () => resolve(), { once: true });
  });
}

/** Poll `predicate` at 1ms granularity; throw rather than hang on failure. */
async function waitFor(
  predicate: () => boolean | Promise<boolean>,
  what: string,
  timeoutMs = 5_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await predicate()) return;
    if (Date.now() > deadline) {
      throw new Error(`timed out after ${timeoutMs}ms waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

/** Give the runner's claim loop several poll cycles to prove it does NOT do something. */
function settle(ms = 50): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Renewal interval the lease-renewal scenario asks its harness for, in REAL
 * milliseconds. Short enough that many beats fit inside that scenario's waits,
 * and asked for by that scenario ALONE — every other one needs renewal off.
 */
const RENEWAL_HEARTBEAT_MS = 5;

/**
 * The full TaskRunner-conformance suite. Call once per runner with a fresh
 * `create()` factory; every `it()` below builds its own runner via `create()`,
 * releases anything it left blocked, and stops the worker when done — so tests
 * never share state and a failed assertion cannot leave a claim loop running.
 */
export function describeTaskRunnerConformance(
  options: DescribeTaskRunnerConformanceOptions,
): void {
  const { name, create, test } = options;
  const { describe, it, expect } = test;

  const attemptsFor = async (
    harness: TaskRunnerConformanceHarness,
    taskId: string,
  ) => {
    const attempts = await harness.attemptsFor(taskId);
    return attempts
      .slice()
      .sort((a, b) => a.attemptNumber - b.attemptNumber)
      .map((attempt) => ({
        status: attempt.status,
        attemptNumber: attempt.attemptNumber,
      }));
  };

  const statusOf = async (
    harness: TaskRunnerConformanceHarness,
    taskId: string,
  ) => (await harness.store.tasks.getTask(taskId))?.status;

  describe(`TaskRunner conformance — ${name}`, () => {
    it("enqueue is idempotent per taskId: a re-delivered poke never runs a task twice", async () => {
      const harness = await create();
      const worker = createWorker(
        harness.store,
        () => "hold",
        harness.clock.nowIso,
      );
      const handle = await harness.runner.startWorker(worker, {
        concurrency: 2,
      });
      try {
        await harness.seedTask({ taskId: "t1", scopeId: "s1" });
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await waitFor(
          () => worker.callsFor("t1").length === 1,
          "the first execution to start",
        );

        // The task has LEFT `queued`. Every re-delivery below is a no-op by
        // contract — not an error, and not a second execution.
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await settle();
        expect(worker.callsFor("t1").length).toBe(1);

        worker.releaseAll();
        await waitFor(
          async () => (await statusOf(harness, "t1")) === "completed",
          "t1 to complete",
        );

        // Terminal now: a late duplicate must not resurrect it either.
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await settle();
        expect(worker.callsFor("t1").length).toBe(1);
        expect(await statusOf(harness, "t1")).toBe("completed");
        expect(await attemptsFor(harness, "t1")).toEqual([
          { status: "completed", attemptNumber: 1 },
        ]);
      } finally {
        worker.releaseAll();
        await handle.stop();
        harness.close?.();
      }
    });

    it("recover() abandons an expired lease and gives the task a fresh attempt", async () => {
      const harness = await create();
      // "never" is the process that died holding a lease: the attempt never
      // ends and the lease is never released, which is the only evidence a
      // crash leaves behind.
      const worker = createWorker(
        harness.store,
        (_taskId, index) => (index === 0 ? "never" : "complete"),
        harness.clock.nowIso,
      );
      const handle = await harness.runner.startWorker(worker, {
        concurrency: 2,
      });
      try {
        await harness.seedTask({ taskId: "t1", scopeId: "s1" });
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await waitFor(
          () => worker.callsFor("t1").length === 1,
          "the first execution to start",
        );

        harness.clock.advance(harness.leaseTtlMs + 1);
        await harness.runner.recover();

        // The SAME task continues on a new attempt — not a new task, and not a
        // trip back through `queued`.
        await waitFor(
          () => worker.callsFor("t1").length === 2,
          "a second attempt after recovery",
        );
        const attempts = await attemptsFor(harness, "t1");
        expect(attempts.length).toBe(2);
        expect(attempts[0]).toEqual({
          status: "abandoned",
          attemptNumber: 1,
        });
        const task = await harness.store.tasks.getTask("t1");
        expect(task?.attemptCount).toBe(2);
        expect(task?.deadLetteredAt).toBeUndefined();

        await waitFor(
          async () => (await statusOf(harness, "t1")) === "completed",
          "t1 to complete on its second attempt",
        );
      } finally {
        worker.releaseAll();
        await handle.stop();
        harness.close?.();
      }
    });

    it("renews its lease so an attempt outliving the TTL is not abandoned", async () => {
      // The port says a runner must heartbeat the lease of an attempt it is
      // executing, and every OTHER scenario here is blind to whether it does:
      // they all end the attempt well inside one TTL, so a `renewLease` that
      // did nothing at all would pass the entire suite. It is the difference
      // between a five-minute provider call finishing and being torn out from
      // under itself by the next recovery pass.
      //
      // The observation is a real recovery pass, because that is the only thing
      // that ever asks the store whether a lease is still live: a renewed lease
      // is invisible to it, an un-renewed one is abandoned and re-dispatched.
      const harness = await create({ heartbeatMs: RENEWAL_HEARTBEAT_MS });
      const worker = createWorker(
        harness.store,
        () => "hold",
        harness.clock.nowIso,
      );
      const handle = await harness.runner.startWorker(worker, {
        concurrency: 1,
      });
      try {
        await harness.seedTask({ taskId: "t1", scopeId: "s1" });
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await waitFor(
          () => worker.callsFor("t1").length === 1,
          "the execution to start",
        );

        // Walk the clock three whole TTLs forward while the attempt runs, in
        // steps short enough that a renewal always lands between them. The real
        // waits are what give the heartbeat time to fire; they are many times
        // the interval the harness was built with, not a guess.
        const step = Math.max(1, Math.floor(harness.leaseTtlMs * 0.6));
        for (let round = 0; round < 5; round += 1) {
          harness.clock.advance(step);
          await settle(RENEWAL_HEARTBEAT_MS * 10);
        }

        await harness.runner.recover();
        await settle();
        // Untouched: still one execution, still one attempt, and that attempt
        // was never abandoned.
        expect(worker.callsFor("t1").length).toBe(1);
        const midway = await attemptsFor(harness, "t1");
        expect(midway.length).toBe(1);
        expect(midway[0]?.status).not.toBe("abandoned");

        worker.releaseAll();
        await waitFor(
          async () => (await statusOf(harness, "t1")) === "completed",
          "t1 to complete on its FIRST attempt",
        );
        expect(await attemptsFor(harness, "t1")).toEqual([
          { status: "completed", attemptNumber: 1 },
        ]);
        expect(worker.callsFor("t1").length).toBe(1);
      } finally {
        worker.releaseAll();
        await handle.stop();
        harness.close?.();
      }
    });

    it("recover() dead-letters and fails a task once its attempts are exhausted", async () => {
      const harness = await create();
      const worker = createWorker(
        harness.store,
        () => "never",
        harness.clock.nowIso,
      );
      const handle = await harness.runner.startWorker(worker, {
        concurrency: 2,
      });
      try {
        await harness.seedTask({ taskId: "t1", scopeId: "s1" });
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });

        // One recovery pass per attempt the runner allows. The last one finds a
        // task that has burned its budget and stops feeding it work.
        for (let round = 1; round <= harness.maxAttempts; round += 1) {
          await waitFor(
            () => worker.callsFor("t1").length === round,
            `attempt ${round} to start`,
          );
          harness.clock.advance(harness.leaseTtlMs + 1);
          await harness.runner.recover();
        }

        await waitFor(
          async () => (await statusOf(harness, "t1")) === "failed",
          "t1 to fail once its attempts were exhausted",
        );
        const task = await harness.store.tasks.getTask("t1");
        expect(task?.deadLetteredAt).toBeDefined();
        expect(typeof task?.deadLetterReason).toBe("string");
        expect(task?.attemptCount).toBe(harness.maxAttempts);
        // No attempt beyond the budget was ever handed to the worker.
        expect(worker.callsFor("t1").length).toBe(harness.maxAttempts);
      } finally {
        worker.releaseAll();
        await handle.stop();
        harness.close?.();
      }
    });

    it("requestCancel aborts the running worker's signal and lands the task cancelled", async () => {
      const harness = await create();
      const worker = createWorker(
        harness.store,
        () => "await-abort",
        harness.clock.nowIso,
      );
      const handle = await harness.runner.startWorker(worker, {
        concurrency: 2,
      });
      try {
        await harness.seedTask({ taskId: "t1", scopeId: "s1" });
        await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
        await waitFor(
          () => worker.callsFor("t1").length === 1,
          "the execution to start",
        );
        expect(worker.calls[0]?.signal.aborted).toBe(false);

        await harness.runner.requestCancel("t1");

        await waitFor(
          () => worker.abortedCount() === 1,
          "the execution's signal to abort",
        );
        await waitFor(
          async () => (await statusOf(harness, "t1")) === "cancelled",
          "t1 to land cancelled",
        );
        expect(await attemptsFor(harness, "t1")).toEqual([
          { status: "cancelled", attemptNumber: 1 },
        ]);
        // A cancelled task is terminal: nothing re-dispatches it.
        await settle();
        expect(worker.callsFor("t1").length).toBe(1);
      } finally {
        worker.releaseAll();
        await handle.stop();
        harness.close?.();
      }
    });

    it("requestCancel on a queued task cancels it before any worker sees it", async () => {
      const harness = await create();
      const worker = createWorker(
        harness.store,
        () => "hold",
        harness.clock.nowIso,
      );
      try {
        await harness.seedTask({ taskId: "t1", scopeId: "s1" });
        // No worker started: nothing can claim it out from under the cancel.
        await harness.runner.requestCancel("t1");
        expect(await statusOf(harness, "t1")).toBe("cancelled");

        const handle = await harness.runner.startWorker(worker, {
          concurrency: 2,
        });
        try {
          await harness.runner.enqueue({ taskId: "t1", scopeId: "s1" });
          await settle();
          expect(worker.callsFor("t1").length).toBe(0);
          expect(await attemptsFor(harness, "t1")).toEqual([]);
        } finally {
          worker.releaseAll();
          await handle.stop();
        }
      } finally {
        harness.close?.();
      }
    });

    it("startWorker respects concurrency: never more attempts in flight than the budget", async () => {
      const harness = await create();
      const worker = createWorker(
        harness.store,
        () => "hold",
        harness.clock.nowIso,
      );
      const handle = await harness.runner.startWorker(worker, {
        concurrency: 2,
      });
      try {
        // Distinct scopes: same-scope serialization would hide a broken budget
        // behind a lock that happens to produce the same number.
        for (const id of ["t1", "t2", "t3", "t4"]) {
          await harness.seedTask({ taskId: id, scopeId: `s-${id}` });
          await harness.runner.enqueue({ taskId: id, scopeId: `s-${id}` });
        }

        await waitFor(() => worker.calls.length === 2, "two executions");
        await settle();
        // The third and fourth are still queued behind the budget, not running.
        expect(worker.calls.length).toBe(2);
        expect(worker.running()).toBe(2);
        expect(worker.peak()).toBe(2);

        worker.releaseAll();
        await waitFor(async () => {
          const statuses = await Promise.all(
            ["t1", "t2", "t3", "t4"].map((id) => statusOf(harness, id)),
          );
          if (statuses.every((status) => status === "completed")) return true;
          // Whatever the loop just picked up also has to be released.
          worker.releaseAll();
          return false;
        }, "all four tasks to complete");

        expect(worker.calls.length).toBe(4);
        expect(worker.peak()).toBeLessThanOrEqual(2);
      } finally {
        worker.releaseAll();
        await handle.stop();
        harness.close?.();
      }
    });
  });
}
