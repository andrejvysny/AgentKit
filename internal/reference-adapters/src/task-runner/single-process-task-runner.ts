/**
 * A complete {@link TaskRunner} for one process: claim, execute, heartbeat,
 * retry, dead-letter, recover.
 *
 * It owns no state a restart needs. Everything that decides what happens next —
 * task status, attempt history, lease ownership, the fencing token — lives in
 * the {@link AssistantStore}. What lives here is scheduling: which claimed task
 * to hand the worker, when to try again, and when to stop trying. Kill this
 * process mid-task and the next one's `recover()` finds the expired lease and
 * continues the SAME task, on a new attempt, with the event sequence unbroken.
 *
 * Three things it deliberately does differently from the task-system runtime it
 * salvages ideas from:
 *
 * 1. DISPATCH IS FIRE-AND-FORGET. task-system's `processQueue` awaited its
 *    own callback, so "concurrency 3" executed exactly one task at a time — the
 *    whole queue serialized behind a bug that looked like a loop. Here the claim
 *    loop NEVER awaits an execution; it starts one, keeps claiming until the
 *    concurrency budget is spent, and sleeps.
 * 2. RETRY IS CLASSIFIED. See {@link classifyExecutionError} — unknown failures
 *    are terminal, not retried forever.
 * 3. RETRY HAPPENS IN PLACE. {@link TASK_TRANSITIONS} has no `running → queued`
 *    edge, on purpose: a task that has started is not "waiting to start" again,
 *    and pretending otherwise would make it claimable by a second worker while
 *    the first is still landing. So a retry creates a NEW ATTEMPT (with a new
 *    lease, so the previous attempt's token is fenced out) on a task that stays
 *    `running` for its whole life. The task's terminal transition happens once.
 *
 * SINGLE-PROCESS LIMITS, which belong to a distributed adapter rather than to
 * this one: cancellation of a RUNNING task is delivered in memory, by aborting
 * the `AbortController` this process registered for it. A cancel aimed at a task
 * some other process is executing does nothing here; a durable cross-process
 * cancel needs a cancellation flag in the store that the worker polls, which is
 * a different design with a different cost. Likewise
 * {@link ScopeLock} is per-process — correctness against concurrent processes
 * rests on the store's `claimNext` + leases, never on the lock.
 */
import {
  AgentKitHostError,
  LeaseLostError,
  RecordNotFoundError,
  defaultClock,
  type AssistantStore,
  type AttemptRecord,
  type ClaimedTask,
  type Clock,
  type EnqueueInput,
  type Lease,
  type Logger,
  type StartWorkerOptions,
  type TaskRunner,
  type TaskWorker,
  type WorkerHandle,
} from "@agentkit/host";
import { classifyExecutionError, errorMessage } from "./error-classifier.js";
import { ScopeLock } from "./scope-lock.js";

const DEFAULT_LEASE_TTL_MS = 30_000;
const DEFAULT_HEARTBEAT_MS = 10_000;
const DEFAULT_POLL_MS = 100;
const DEFAULT_MAX_ATTEMPTS = 3;
const DEFAULT_CONCURRENCY = 2;

/** Dead-letter reason for a task whose attempts died without ever landing. */
const POISON_REASON = "poison: attempts exhausted without a terminal outcome";

export interface SingleProcessTaskRunnerDeps {
  store: AssistantStore;
  /** Defaults to {@link defaultClock}. Injectable so lease expiry is testable. */
  clock?: Clock;
  logger?: Logger;
  /** Lease lifetime granted to each attempt this runner creates. Default 30s. */
  leaseTtlMs?: number;
  /** Renewal interval while an attempt runs. Must be well under the TTL. Default 10s. */
  heartbeatMs?: number;
  /** Idle delay between claim attempts. Default 100ms. */
  pollMs?: number;
  /** Attempts per task before it is dead-lettered. Default 3. */
  maxAttempts?: number;
}

/** What one {@link SingleProcessTaskRunner.recover} pass did. */
export interface RecoveryReport {
  /** Leases found past their expiry. */
  expired: number;
  /** Tasks given a fresh attempt and handed back to the worker. */
  redispatched: number;
  /** Tasks that had burned their attempt budget and were dead-lettered. */
  deadLettered: number;
}

/** One task this process is executing right now. */
interface ActiveExecution {
  taskId: string;
  scopeId: string;
  /** Replaced per attempt: a retry must not inherit an aborted signal. */
  controller: AbortController;
  /** A cancel arrived; do not retry, and land the task `cancelled`. */
  cancelRequested: boolean;
}

/** What one attempt decided: stop here, or run again under `next`. */
type AttemptOutcome =
  | { kind: "done" }
  | { kind: "retry"; next: { attemptId: string; lease: Lease } };

export class SingleProcessTaskRunner implements TaskRunner {
  /**
   * Per-scope serialization + queue positions. Public because "you are second
   * in line for this chat" is something a host wants to render, and because it
   * is pure in-memory scheduling state — reading it cannot corrupt anything.
   */
  readonly scopeLock = new ScopeLock();

  private readonly store: AssistantStore;
  private readonly clock: Clock;
  private readonly logger: Logger | undefined;
  private readonly leaseTtlMs: number;
  private readonly heartbeatMs: number;
  private readonly pollMs: number;
  private readonly maxAttempts: number;

  /** taskId → the execution this process is running for it. */
  private readonly active = new Map<string, ActiveExecution>();
  /** Every un-settled execution promise, so `stop()` can wait them out. */
  private readonly inFlight = new Set<Promise<void>>();

  private worker: TaskWorker | null = null;
  private ownerId = "";
  private concurrency = DEFAULT_CONCURRENCY;
  private stopped = false;
  private timer: ReturnType<typeof setTimeout> | null = null;
  /** True while a claim pass is running — there is exactly one loop. */
  private ticking = false;
  /** A wake-up arrived mid-pass; claim again immediately instead of sleeping. */
  private wakeRequested = false;

  constructor(deps: SingleProcessTaskRunnerDeps) {
    this.store = deps.store;
    this.clock = deps.clock ?? defaultClock;
    this.logger = deps.logger;
    this.leaseTtlMs = deps.leaseTtlMs ?? DEFAULT_LEASE_TTL_MS;
    this.heartbeatMs = deps.heartbeatMs ?? DEFAULT_HEARTBEAT_MS;
    this.pollMs = deps.pollMs ?? DEFAULT_POLL_MS;
    this.maxAttempts = deps.maxAttempts ?? DEFAULT_MAX_ATTEMPTS;
  }

  // ───────────────────────────── TaskRunner ──────────────────────────────

  /**
   * Wake the claim loop for a task that is ALREADY persisted.
   *
   * This runner does not create the task row: the host wrote it (queued) in the
   * same transaction as the records it belongs to — that transaction is what
   * makes "the user hit send" survive a crash, and re-deriving the row here
   * would be a second, weaker source of truth. So `enqueue` validates and pokes.
   *
   * Idempotent per the port contract: a re-delivered enqueue for a task that is
   * no longer `queued` is a silent no-op, never a second execution of one task.
   * `priority` / `availableAt` on the input are NOT re-applied — the store owns
   * them, and the port has no `queued → queued` patch to write them through.
   */
  async enqueue(input: EnqueueInput): Promise<void> {
    const task = await this.store.tasks.getTask(input.taskId);
    if (!task) {
      throw new RecordNotFoundError(`Task not found: ${input.taskId}`, {
        taskId: input.taskId,
      });
    }
    if (task.scopeId !== input.scopeId) {
      // The stored scope wins: it is what `claimNext` serializes on.
      this.logger?.warn("enqueue scopeId disagrees with the stored task", {
        taskId: input.taskId,
        enqueued: input.scopeId,
        stored: task.scopeId,
      });
    }
    if (task.status !== "queued") {
      this.logger?.debug("enqueue ignored: task is not queued", {
        taskId: task.taskId,
        status: task.status,
      });
      return;
    }
    // Record the queue position ONLY when something is already executing in the
    // scope. Marking a free scope busy for a task nobody has claimed yet would
    // make `claimNext` skip the very task we are trying to start.
    if (this.scopeLock.hasActive(task.scopeId)) {
      this.scopeLock.tryAcquire(task.scopeId, task.taskId);
    }
    this.kick();
  }

  /**
   * Queued task: cancelled outright, before any worker sees it. Running task:
   * the execution's signal is aborted and the worker decides how to land
   * (TurnRunner emits `run.cancelled` and finalizes the task itself; this runner
   * then sees a terminal task and writes nothing more).
   *
   * A running task with no local execution means another process owns it — see
   * the class doc on the single-process limit. `recover()` reconciles it once
   * that owner's lease expires.
   */
  async requestCancel(taskId: string): Promise<void> {
    const task = await this.store.tasks.getTask(taskId);
    if (!task) return;

    if (task.status === "queued" || task.status === "waiting_approval") {
      // `waiting_approval` is included because the transition table allows it
      // and nothing else in this runner can reach it: a task parked on a human
      // is exactly the kind a user cancels.
      try {
        await this.store.tasks.transitionTask(
          taskId,
          [task.status],
          "cancelled",
          { finishedAt: this.clock.nowIso() },
        );
        // Only after the store agreed: if a claim beat us to it, this task now
        // HOLDS the scope, and clearing the lock would let a second task in the
        // same scope start alongside it.
        this.scopeLock.remove(task.scopeId, taskId);
      } catch (err) {
        // Lost the race: a claim flipped it to `running` between the read and
        // the write. Fall through to the abort path rather than reporting a
        // cancel that did not happen.
        this.logger?.debug("cancel raced a claim; aborting instead", {
          taskId,
          error: errorMessage(err),
        });
        this.abortActive(taskId);
      }
      return;
    }

    if (task.status === "running") {
      if (!this.abortActive(taskId)) {
        this.logger?.info(
          "cancel for a running task with no local execution; recover() will reconcile",
          { taskId },
        );
      }
      return;
    }
    // Terminal: nothing to stop.
  }

  /**
   * Startup pass over leases whose owner died.
   *
   * `expireStaleLeases` is the only evidence available that a process stopped
   * mid-attempt — there is no callback from a crash. Each expired lease ends its
   * attempt as `abandoned` (not `failed`: nobody knows whether the work
   * succeeded), and its task, still `running`, either gets a fresh attempt or is
   * dead-lettered for having burned its budget.
   *
   * Safe to call before `startWorker`; with no worker started there is nobody to
   * hand the work to, so abandoned tasks are left for the next owner's `recover`
   * (the report says how many).
   *
   * NOT done here: reconciling a proposal apply that a crash interrupted. That
   * is already idempotent by construction — `ProposalStore.recordOutcome` is
   * keyed on `operationId`, so a replayed apply returns the recorded outcome
   * instead of re-running side effects — and re-deriving it from the queue side
   * would be a second, weaker guard over the same key.
   *
   * The port declares `Promise<void>`, and TypeScript will not let an
   * implementation narrow that to `Promise<RecoveryReport>` — so the pass itself
   * lives in {@link recoverWithReport} and this method discards its summary.
   * Callers that want the numbers (a startup log line, a test) call that one.
   */
  async recover(): Promise<void> {
    await this.recoverWithReport();
  }

  /** {@link recover}, with a summary of what the pass did. */
  async recoverWithReport(): Promise<RecoveryReport> {
    const expired = await this.store.tasks.expireStaleLeases(this.clock.now());
    const report: RecoveryReport = {
      expired: expired.length,
      redispatched: 0,
      deadLettered: 0,
    };
    const worker = this.worker;

    for (const lease of expired) {
      const task = await this.store.tasks.getTask(lease.taskId);
      if (!task) continue;

      // A terminal task means its worker finalized the attempt before dying (or
      // the lease simply outlived a clean finish). Its recorded outcome is the
      // truth; overwriting it with `abandoned` would be a lie about what
      // happened.
      if (task.status !== "running") continue;

      try {
        await this.store.tasks.endAttempt({
          attemptId: lease.attemptId,
          status: "abandoned",
          error: "lease expired",
        });
      } catch (err) {
        this.logger?.warn("could not end an abandoned attempt", {
          taskId: task.taskId,
          attemptId: lease.attemptId,
          error: errorMessage(err),
        });
      }

      if (task.attemptCount >= this.maxAttempts) {
        await this.deadLetter(task.taskId, POISON_REASON);
        report.deadLettered += 1;
        continue;
      }

      if (!worker || this.stopped) {
        this.logger?.info("abandoned task left for the next owner to recover", {
          taskId: task.taskId,
          attempts: task.attemptCount,
        });
        continue;
      }

      const next = await this.startAttempt(task.taskId);
      // The re-dispatch continues the SAME task: same id, same event sequence,
      // one more attempt.
      this.dispatch({ task, attempt: next.attempt, lease: next.lease }, worker);
      report.redispatched += 1;
    }

    return report;
  }

  /**
   * Start claiming. One worker per runner instance: a second would race this
   * one's `active` map and concurrency budget for no benefit — run two runners
   * if two workers are wanted.
   */
  async startWorker(
    worker: TaskWorker,
    opts: StartWorkerOptions = {},
  ): Promise<WorkerHandle> {
    if (this.worker) {
      throw new AgentKitHostError(
        "worker_already_started",
        "This runner already has a worker; stop it before starting another.",
      );
    }
    this.worker = worker;
    this.ownerId = opts.ownerId ?? `owner_${crypto.randomUUID()}`;
    this.concurrency = Math.max(1, opts.concurrency ?? DEFAULT_CONCURRENCY);
    this.stopped = false;
    this.kick();
    return { stop: () => this.stop() };
  }

  // ────────────────────────────── dispatch ───────────────────────────────

  /** Stop claiming, then wait for everything in flight to settle. */
  private async stop(): Promise<void> {
    this.stopped = true;
    this.worker = null;
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    // Loop rather than a single `allSettled`: an execution can spawn a retry
    // (a new promise) while we are waiting on it.
    while (this.inFlight.size > 0) {
      await Promise.allSettled([...this.inFlight]);
    }
  }

  /** Claim immediately instead of waiting out the poll interval. */
  private kick(): void {
    if (this.stopped || !this.worker) return;
    if (this.ticking) {
      this.wakeRequested = true;
      return;
    }
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    this.schedule(0);
  }

  private schedule(delayMs: number): void {
    if (this.stopped || !this.worker) return;
    if (this.timer) return;
    const timer = setTimeout(() => {
      this.timer = null;
      void this.tick();
    }, delayMs);
    // Never hold the process open for a poll that has nothing to poll for.
    (timer as unknown as { unref?: () => void }).unref?.();
    this.timer = timer;
  }

  /**
   * One claim pass: take work until the concurrency budget is spent or the store
   * has nothing claimable, then re-arm.
   *
   * The executions started here are NOT awaited. That is the whole point — see
   * the class doc. Awaiting even one of them would serialize the queue behind
   * the slowest task, which is precisely the task-system bug this replaces.
   */
  private async tick(): Promise<void> {
    const worker = this.worker;
    if (this.stopped || !worker) return;
    if (this.ticking) {
      this.wakeRequested = true;
      return;
    }
    this.ticking = true;
    try {
      while (!this.stopped && this.active.size < this.concurrency) {
        let claimed: ClaimedTask | null;
        try {
          claimed = await this.store.tasks.claimNext({
            ownerId: this.ownerId,
            now: this.clock.now(),
            scopesBusy: this.scopeLock.busyScopes(),
          });
        } catch (err) {
          this.logger?.error("claimNext failed", { error: errorMessage(err) });
          break;
        }
        if (!claimed) break;
        this.dispatch(claimed, worker);
      }
    } finally {
      this.ticking = false;
    }
    const immediate = this.wakeRequested;
    this.wakeRequested = false;
    this.schedule(immediate ? 0 : this.pollMs);
  }

  /**
   * Register an execution and start it without waiting for it.
   *
   * Registration is synchronous so the claim loop's `active.size` budget check
   * on the very next iteration already accounts for this task.
   */
  private dispatch(claimed: ClaimedTask, worker: TaskWorker): void {
    const { task } = claimed;
    const superseded = this.active.get(task.taskId);
    if (superseded) {
      // Only reachable via `recover()` re-dispatching a task whose previous
      // execution is still hanging in this process. The new one owns the task
      // from here; the old one is fenced out by the lease and will write
      // nothing when it eventually settles.
      this.logger?.warn("superseding a still-running execution after recovery", {
        taskId: task.taskId,
      });
    }
    if (!this.scopeLock.tryAcquire(task.scopeId, task.taskId)) {
      // Should not happen: the claim passed `scopesBusy` from this same lock.
      // The store already handed us the lease, so refusing to run would strand
      // the task in `running` with nobody executing it — log and proceed.
      this.logger?.warn("claimed a task whose scope was locally busy", {
        taskId: task.taskId,
        scopeId: task.scopeId,
        holder: this.scopeLock.activeRun(task.scopeId),
      });
    }
    const entry: ActiveExecution = {
      taskId: task.taskId,
      scopeId: task.scopeId,
      controller: new AbortController(),
      cancelRequested: false,
    };
    this.active.set(task.taskId, entry);

    const promise = this.executeClaimed(claimed, worker, entry);
    this.inFlight.add(promise);
    void promise.finally(() => {
      this.inFlight.delete(promise);
    });
  }

  /**
   * Drive one task to a terminal state, attempt after attempt.
   *
   * The retry loop lives HERE rather than going back through the claim loop, and
   * holds the scope lock and its concurrency slot for the whole chain: the task
   * never returns to `queued`, so there is nothing for a claim to pick up. See
   * the class doc's point 3.
   */
  private async executeClaimed(
    claimed: ClaimedTask,
    worker: TaskWorker,
    entry: ActiveExecution,
  ): Promise<void> {
    const { taskId, scopeId } = entry;
    let attemptId = claimed.attempt.attemptId;
    let lease = claimed.lease;
    try {
      for (;;) {
        const outcome = await this.runAttempt(
          taskId,
          attemptId,
          lease,
          entry,
          worker,
        );
        if (outcome.kind === "done") break;
        attemptId = outcome.next.attemptId;
        lease = outcome.next.lease;
      }
    } catch (err) {
      // Bookkeeping itself failed (the store threw while landing the task). The
      // task stays `running` with a live lease, which is recoverable: the lease
      // expires and `recover()` picks it up. Losing the loop would not be.
      this.logger?.error("task execution bookkeeping failed", {
        taskId,
        attemptId,
        error: errorMessage(err),
      });
    } finally {
      // Best effort: the token may already have been replaced (a retry) or
      // expired (a takeover), and both throw here.
      try {
        await this.store.tasks.releaseLease(lease.leaseToken);
      } catch {
        /* not ours any more — nothing to release */
      }
      // A superseded execution (recovery re-dispatched its task while it hung)
      // must not release resources the new execution now owns.
      if (this.active.get(taskId) === entry) {
        this.active.delete(taskId);
        const successor = this.scopeLock.release(scopeId, taskId);
        if (successor !== null) {
          this.logger?.debug("scope freed for the next waiter", {
            scopeId,
            taskId: successor,
          });
        }
        this.kick();
      }
    }
  }

  /**
   * One attempt: heartbeat, execute, then decide what the outcome means.
   *
   * Every durable write below is gated on a fencing check first. An attempt that
   * lost its lease while running is a zombie — some other owner has the task
   * now, and a write from here would interleave with theirs or overwrite a
   * verdict they already recorded.
   */
  private async runAttempt(
    taskId: string,
    attemptId: string,
    lease: Lease,
    entry: ActiveExecution,
    worker: TaskWorker,
  ): Promise<AttemptOutcome> {
    entry.controller = new AbortController();
    if (entry.cancelRequested) entry.controller.abort();

    const heartbeat = setInterval(() => {
      void this.renewLease(taskId, lease.leaseToken, entry);
    }, this.heartbeatMs);
    (heartbeat as unknown as { unref?: () => void }).unref?.();

    let thrown: unknown;
    let threw = false;
    try {
      await worker.execute({
        taskId,
        attemptId,
        leaseToken: lease.leaseToken,
        signal: entry.controller.signal,
      });
    } catch (err) {
      threw = true;
      thrown = err;
    } finally {
      clearInterval(heartbeat);
    }

    if (!(await this.stillHoldsLease(lease.leaseToken))) {
      this.logger?.warn("attempt finished without its lease; writing nothing", {
        taskId,
        attemptId,
      });
      return { kind: "done" };
    }

    return threw
      ? await this.settleThrown(taskId, attemptId, entry, thrown)
      : await this.settleResolved(taskId, attemptId, entry);
  }

  /**
   * The worker returned. Normally it has already finalized the task itself
   * (TurnRunner transitions the task and ends its own attempt on its way out),
   * in which case there is nothing left to do — the task is no longer `running`
   * and this method writes nothing.
   *
   * The defensive branch below is for a worker that returns without landing the
   * task: leaving it `running` forever with no lease would make it invisible to
   * both the claim loop (which only takes `queued`) and `recover()` (which only
   * sees expired leases).
   */
  private async settleResolved(
    taskId: string,
    attemptId: string,
    entry: ActiveExecution,
  ): Promise<AttemptOutcome> {
    const task = await this.store.tasks.getTask(taskId);
    if (!task || task.status !== "running") return { kind: "done" };
    // A cancel that the worker swallowed still means cancelled, not completed.
    const status = entry.cancelRequested ? "cancelled" : "completed";
    await this.store.tasks.endAttempt({ attemptId, status });
    await this.store.tasks.transitionTask(taskId, ["running"], status, {
      finishedAt: this.clock.nowIso(),
    });
    return { kind: "done" };
  }

  /** The worker threw: classify, then retry / dead-letter / fail / cancel. */
  private async settleThrown(
    taskId: string,
    attemptId: string,
    entry: ActiveExecution,
    err: unknown,
  ): Promise<AttemptOutcome> {
    const classified = classifyExecutionError(err);
    const kind = entry.cancelRequested ? "cancelled" : classified.kind;
    const message = errorMessage(err);

    if (kind === "cancelled") {
      await this.store.tasks.endAttempt({
        attemptId,
        status: "cancelled",
        error: message,
      });
      await this.landIfRunning(taskId, "cancelled");
      return { kind: "done" };
    }

    await this.store.tasks.endAttempt({
      attemptId,
      status: "failed",
      error: `${classified.reason}: ${message}`,
    });

    if (kind === "terminal") {
      // NOT dead-lettered: the dead-letter row means "this poisoned the queue,
      // stop feeding it work". A cleanly-diagnosed failure — an unregistered
      // kind, a rejected request — is just a failure.
      this.logger?.info("task failed with a terminal error", {
        taskId,
        attemptId,
        reason: classified.reason,
      });
      await this.landIfRunning(taskId, "failed", `${classified.reason}: ${message}`);
      return { kind: "done" };
    }

    // Transient from here down.
    const task = await this.store.tasks.getTask(taskId);
    if (!task || task.status !== "running") {
      // A worker that lands the task itself before rethrowing (TurnRunner's
      // `failQuietly` does exactly this) has already spent the task's life; the
      // transition table has no way back from `failed`, and inventing one to
      // force a retry would resurrect a task the worker deliberately buried.
      // Such a worker opts out of runner-level retries by construction — its
      // own retry policy lives inside the executor.
      this.logger?.debug("no retry: the worker already landed the task", {
        taskId,
        status: task?.status,
      });
      return { kind: "done" };
    }

    // `attemptCount` is the store's own count, incremented by `createAttempt` —
    // no separate bookkeeping to drift out of sync with the attempt rows.
    if (task.attemptCount >= this.maxAttempts) {
      await this.deadLetter(taskId, `${classified.reason}: ${message}`);
      return { kind: "done" };
    }
    if (this.stopped || !this.worker) {
      // Shutting down mid-retry: leave the task `running` with a live lease that
      // will expire, so the next owner's `recover()` continues it.
      this.logger?.info("retry deferred to recovery: the worker is stopping", {
        taskId,
        attempts: task.attemptCount,
      });
      return { kind: "done" };
    }

    this.logger?.info("retrying task in place", {
      taskId,
      attempt: task.attemptCount + 1,
      reason: classified.reason,
    });
    const next = await this.startAttempt(taskId);
    return {
      kind: "retry",
      next: { attemptId: next.attempt.attemptId, lease: next.lease },
    };
  }

  // ────────────────────────────── store ops ──────────────────────────────

  /**
   * A fresh attempt + lease on a task that stays `running`.
   *
   * Acquiring replaces the run's lease and draws the next fencing token, so the
   * previous attempt's `leaseToken` stops being accepted by `appendEvents` from
   * this moment — that is what stops a zombie from interleaving its events into
   * the live attempt's stream.
   */
  private async startAttempt(
    taskId: string,
  ): Promise<{ attempt: AttemptRecord; lease: Lease }> {
    const attempt = await this.store.tasks.createAttempt({
      attemptId: `att_${crypto.randomUUID()}`,
      taskId,
      ownerId: this.ownerId,
    });
    const lease = await this.store.tasks.acquireLease({
      taskId,
      attemptId: attempt.attemptId,
      ownerId: this.ownerId,
      ttlMs: this.leaseTtlMs,
    });
    return { attempt, lease };
  }

  /** Mark the task poisoned AND land it failed — the row alone stops nothing. */
  private async deadLetter(taskId: string, reason: string): Promise<void> {
    this.logger?.warn("dead-lettering task", { taskId, reason });
    await this.store.tasks.markDeadLettered(taskId, reason);
    await this.landIfRunning(taskId, "failed", reason);
  }

  /**
   * Transition a task that is still `running`, tolerating the race where it is
   * not: the worker may have finalized it (TurnRunner does), or a cancel may
   * have landed first. Re-reading rather than assuming is what keeps the runner
   * from throwing `InvalidTaskTransitionError` at itself on every happy path.
   */
  private async landIfRunning(
    taskId: string,
    to: "completed" | "failed" | "cancelled",
    error?: string,
  ): Promise<void> {
    const task = await this.store.tasks.getTask(taskId);
    if (!task || task.status !== "running") {
      this.logger?.debug("task already terminal; skipping transition", {
        taskId,
        status: task?.status,
        intended: to,
      });
      return;
    }
    await this.store.tasks.transitionTask(taskId, ["running"], to, {
      finishedAt: this.clock.nowIso(),
      ...(error === undefined ? {} : { error }),
    });
  }

  /**
   * Renewal doubles as the fencing probe: the store rejects a token that is no
   * longer current, which is exactly the question "may I still write?".
   */
  private async stillHoldsLease(leaseToken: string): Promise<boolean> {
    try {
      await this.store.tasks.renewLease(leaseToken, this.leaseTtlMs);
      return true;
    } catch (err) {
      if (err instanceof LeaseLostError) return false;
      throw err;
    }
  }

  private async renewLease(
    taskId: string,
    leaseToken: string,
    entry: ActiveExecution,
  ): Promise<void> {
    try {
      await this.store.tasks.renewLease(leaseToken, this.leaseTtlMs);
    } catch (err) {
      if (err instanceof LeaseLostError) {
        // The task belongs to someone else now. Abort so the worker stops
        // burning provider calls on output nobody will accept — and so the
        // fencing check after `execute` has an aborted attempt to report on.
        entry.controller.abort();
        this.logger?.warn("lease lost mid-attempt; aborting the execution", {
          taskId,
          error: errorMessage(err),
        });
        return;
      }
      // A store hiccup is not a lost lease. Let the next heartbeat try again
      // rather than killing a healthy attempt over one failed write.
      this.logger?.warn("lease renewal failed; will retry on the next beat", {
        taskId,
        error: errorMessage(err),
      });
    }
  }

  /** Abort the local execution of `taskId`. False when there is none. */
  private abortActive(taskId: string): boolean {
    const entry = this.active.get(taskId);
    if (!entry) return false;
    entry.cancelRequested = true;
    entry.controller.abort();
    return true;
  }
}

/** Convenience for a host that wants a runner without naming the class. */
export function createSingleProcessTaskRunner(
  deps: SingleProcessTaskRunnerDeps,
): SingleProcessTaskRunner {
  return new SingleProcessTaskRunner(deps);
}
