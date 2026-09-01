/**
 * Test doubles for the task-runner suite.
 *
 * Two rules the whole suite is built on:
 *
 * - TIME IS INJECTED. Lease expiry is the only thing here defined in terms of
 *   wall-clock, and a test that cannot move the clock cannot assert on it. The
 *   {@link TestClock} drives the store's timestamps AND the runner's `now`, so
 *   "the lease expired" is a statement about the fake clock, not a `sleep`.
 * - REAL WAITING STAYS TINY. The dispatch loop is a real timer, so the suite
 *   polls for conditions at 1ms granularity with a hard ceiling instead of
 *   sleeping for a fixed guess.
 */
import type { AiRunEvent } from "@agentkit/contracts";
import { CHAT_TURN_TASK_KIND } from "@agentkit/host";
import type { Clock, TaskExecution, TaskWorker } from "@agentkit/host";
import { createTestEventStamper } from "@agentkit/testing";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SingleProcessTaskRunner } from "../../src/index.js";

/** A clock that starts fixed and only moves when a test says so. */
export interface TestClock extends Clock {
  advance(ms: number): void;
}

export function createTestClock(start = "2026-01-01T00:00:00.000Z"): TestClock {
  let current = new Date(start).getTime();
  return {
    now: () => new Date(current),
    nowIso: () => new Date(current).toISOString(),
    advance: (ms: number) => {
      current += ms;
    },
  };
}

/** Poll `predicate` at 1ms granularity; throw rather than hang on failure. */
export async function waitFor(
  predicate: () => boolean | Promise<boolean>,
  what = "condition",
  timeoutMs = 2_000,
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

/** Give the dispatch loop a few poll cycles to prove it does NOT do something. */
export async function settle(ms = 40): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, ms));
}

/** What one attempt of one task does when the worker is handed it. */
export type AttemptBehavior =
  /** Append an event, then finalize the task `completed` (what TurnRunner does). */
  | { kind: "complete" }
  /** Append an event, then throw — the classifier decides what it meant. */
  | { kind: "throw"; error: unknown }
  /** Block until the test releases this task, then behave like `complete`. */
  | { kind: "hold" }
  /** Block until the signal aborts, then append `run.cancelled` and finalize. */
  | { kind: "await-abort" }
  /** Never resolve, write nothing: the process that died holding a lease. */
  | { kind: "never" };

export interface WorkerCall {
  taskId: string;
  attemptId: string;
  leaseToken: string;
  behavior: AttemptBehavior["kind"];
}

/**
 * A {@link TaskWorker} whose behaviour is scripted per task and per attempt,
 * and which records enough to assert on overlap, ordering, and fencing.
 *
 * The scripted paths write through the store the way the real worker does —
 * `appendEvents` with the attempt's `leaseToken`, then `transitionTask` +
 * `endAttempt` — so a test that says "the worker finalized the task" is testing
 * the runner's tolerance of a self-finalizing worker, not a mock's opinion.
 */
export class FakeWorker implements TaskWorker {
  /** Every execute() call, in order. */
  readonly calls: WorkerCall[] = [];
  /** `start:<taskId>` / `end:<taskId>`, for ordering assertions. */
  readonly timeline: string[] = [];
  /** Highest number of executions running at the same instant. */
  peakConcurrency = 0;

  private running = 0;
  private readonly scripts = new Map<string, AttemptBehavior[]>();
  private readonly holdGates = new Map<string, Array<() => void>>();
  private readonly deadGates: Array<() => void> = [];

  constructor(private readonly store: MemoryAssistantStore) {}

  /** Behaviour per attempt of `taskId`; the LAST entry repeats forever. */
  script(taskId: string, behaviors: AttemptBehavior[]): void {
    this.scripts.set(taskId, behaviors);
  }

  callsFor(taskId: string): WorkerCall[] {
    return this.calls.filter((call) => call.taskId === taskId);
  }

  /** Let the oldest blocked `hold` for this task finish. */
  release(taskId: string): void {
    const gates = this.holdGates.get(taskId);
    const gate = gates?.shift();
    if (!gate) throw new Error(`no held execution for ${taskId}`);
    gate();
  }

  /** Unblock everything, including `never` executions — teardown only. */
  releaseAll(): void {
    for (const gates of this.holdGates.values()) {
      for (const gate of gates.splice(0)) gate();
    }
    for (const gate of this.deadGates.splice(0)) gate();
  }

  async execute(execution: TaskExecution): Promise<void> {
    const { taskId, attemptId, leaseToken, signal } = execution;
    const behavior = this.nextBehavior(taskId);
    this.calls.push({ taskId, attemptId, leaseToken, behavior: behavior.kind });
    this.running += 1;
    this.peakConcurrency = Math.max(this.peakConcurrency, this.running);
    this.timeline.push(`start:${taskId}`);
    try {
      switch (behavior.kind) {
        case "complete":
          await this.appendCompleted(taskId, attemptId, leaseToken);
          await this.finish(taskId, attemptId, "completed");
          return;
        case "throw":
          await this.appendCompleted(taskId, attemptId, leaseToken);
          throw behavior.error;
        case "hold":
          await this.hold(taskId);
          await this.appendCompleted(taskId, attemptId, leaseToken);
          await this.finish(taskId, attemptId, "completed");
          return;
        case "await-abort":
          await this.untilAborted(signal);
          await this.appendCancelled(taskId, attemptId, leaseToken);
          await this.finish(taskId, attemptId, "cancelled");
          return;
        case "never":
          await new Promise<void>((resolve) => this.deadGates.push(resolve));
          return;
      }
    } finally {
      this.running -= 1;
      this.timeline.push(`end:${taskId}`);
    }
  }

  private nextBehavior(taskId: string): AttemptBehavior {
    const script = this.scripts.get(taskId);
    if (!script || script.length === 0) return { kind: "complete" };
    const index = Math.min(this.callsFor(taskId).length, script.length - 1);
    return script[index]!;
  }

  private hold(taskId: string): Promise<void> {
    return new Promise<void>((resolve) => {
      const gates = this.holdGates.get(taskId) ?? [];
      gates.push(resolve);
      this.holdGates.set(taskId, gates);
    });
  }

  private untilAborted(signal: AbortSignal): Promise<void> {
    if (signal.aborted) return Promise.resolve();
    return new Promise<void>((resolve) => {
      signal.addEventListener("abort", () => resolve(), { once: true });
    });
  }

  private async appendCompleted(
    taskId: string,
    attemptId: string,
    leaseToken: string,
  ): Promise<void> {
    const stamp = createTestEventStamper({
      firstSeq: await this.store.tasks.nextSeq(taskId),
      attemptId,
    });
    const event: AiRunEvent = stamp({
      type: "run.completed",
      runId: taskId,
      timestamp: new Date().toISOString(),
      data: { iterations: 1 },
    });
    await this.store.tasks.appendEvents(taskId, [event], { leaseToken });
  }

  private async appendCancelled(
    taskId: string,
    attemptId: string,
    leaseToken: string,
  ): Promise<void> {
    const stamp = createTestEventStamper({
      firstSeq: await this.store.tasks.nextSeq(taskId),
      attemptId,
    });
    const event: AiRunEvent = stamp({
      type: "run.cancelled",
      runId: taskId,
      timestamp: new Date().toISOString(),
      data: { reason: "requested" },
    });
    await this.store.tasks.appendEvents(taskId, [event], { leaseToken });
  }

  /** Land the task the way TurnRunner does: transition first, then end the attempt. */
  private async finish(
    taskId: string,
    attemptId: string,
    status: "completed" | "cancelled",
  ): Promise<void> {
    await this.store.tasks.transitionTask(taskId, ["running"], status, {
      finishedAt: new Date().toISOString(),
    });
    await this.store.tasks.endAttempt({ attemptId, status });
  }
}

export interface Harness {
  clock: TestClock;
  store: MemoryAssistantStore;
  runner: SingleProcessTaskRunner;
  worker: FakeWorker;
  /** Create the `queued` task row a host would have written in its transaction. */
  seedTask(taskId: string, scopeId?: string): Promise<void>;
  /** Attempt rows for a task, oldest first. */
  attemptsFor(taskId: string): Array<{ status: string; attemptNumber: number }>;
}

export interface HarnessOptions {
  leaseTtlMs?: number;
  heartbeatMs?: number;
  pollMs?: number;
  maxAttempts?: number;
  /**
   * Retry backoff. Defaults to NO delay, because the clock above is frozen: a
   * suite about retry *semantics* would otherwise wait forever for a deadline
   * only a test can advance the clock past. The one suite that is about the
   * delay itself passes its own.
   */
  retryBackoff?: { baseMs?: number; maxMs?: number; jitterRatio?: number };
}

export function createHarness(options: HarnessOptions = {}): Harness {
  const clock = createTestClock();
  const store = new MemoryAssistantStore({
    clock,
    // The TTL `claimNext` stamps on the lease it grants — the runner's own TTL
    // only applies to leases IT acquires (retries, recovery).
    ...(options.leaseTtlMs === undefined
      ? {}
      : { leaseTtlMs: options.leaseTtlMs }),
  });
  const runner = new SingleProcessTaskRunner({
    store,
    clock,
    pollMs: options.pollMs ?? 5,
    // Far beyond any test's real-time lifetime unless a test asks for beats:
    // renewal must not quietly rescue a lease a crash test wants expired.
    heartbeatMs: options.heartbeatMs ?? 60_000,
    ...(options.leaseTtlMs === undefined
      ? {}
      : { leaseTtlMs: options.leaseTtlMs }),
    ...(options.maxAttempts === undefined
      ? {}
      : { maxAttempts: options.maxAttempts }),
    retryBackoff: options.retryBackoff ?? { baseMs: 0, jitterRatio: 0 },
  });
  return {
    clock,
    store,
    runner,
    worker: new FakeWorker(store),
    seedTask: async (taskId: string, scopeId = "chat-1") => {
      await store.tasks.createTask({
        taskId,
        kind: CHAT_TURN_TASK_KIND,
        scopeId,
        payload: { chatId: scopeId },
      });
    },
    attemptsFor: (taskId: string) =>
      [...store.tasks.attempts.values()]
        .filter((attempt) => attempt.taskId === taskId)
        .sort((a, b) => a.attemptNumber - b.attemptNumber)
        .map((attempt) => ({
          status: attempt.status,
          attemptNumber: attempt.attemptNumber,
        })),
  };
}
