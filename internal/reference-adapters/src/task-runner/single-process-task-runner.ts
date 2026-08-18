/**
 * A complete {@link TaskRunner} for one process: claim, execute, heartbeat,
 * retry, dead-letter, recover.
 *
 * It owns no state a restart needs. Everything that decides what happens next —
 * run status, attempt history, lease ownership, the fencing token — lives in the
 * {@link AssistantStore}. What lives here is scheduling: which claimed run to
 * hand the worker, when to try again, and when to stop trying. Kill this process
 * mid-run and the next one's `recover()` finds the expired lease and continues
 * the SAME run, on a new attempt, with the event sequence unbroken.
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
 * 3. RETRY HAPPENS IN PLACE. {@link RUN_TRANSITIONS} has no `running → queued`
 *    edge, on purpose: a run that has started is not "waiting to start" again,
 *    and pretending otherwise would make it claimable by a second worker while
 *    the first is still landing. So a retry creates a NEW ATTEMPT (with a new
 *    lease, so the previous attempt's token is fenced out) on a run that stays
 *    `running` for its whole life. The run's terminal transition happens once.
 *
 * SINGLE-PROCESS LIMITS, which belong to a distributed adapter rather than to
 * this one: cancellation of a RUNNING run is delivered in memory, by aborting
 * the `AbortController` this process registered for it. A cancel aimed at a run
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
  type ClaimedRun,
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

/** Dead-letter reason for a run whose attempts died without ever landing. */
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
  /** Attempts per run before it is dead-lettered. Default 3. */
  maxAttempts?: number;
}

/** What one {@link SingleProcessTaskRunner.recover} pass did. */
export interface RecoveryReport {
  /** Leases found past their expiry. */
  expired: number;
  /** Runs given a fresh attempt and handed back to the worker. */
  redispatched: number;
  /** Runs that had burned their attempt budget and were dead-lettered. */
  deadLettered: number;
}

/** One run this process is executing right now. */
interface ActiveExecution {
  runId: string;
  scopeId: string;
  /** Replaced per attempt: a retry must not inherit an aborted signal. */
  controller: AbortController;
  /** A cancel arrived; do not retry, and land the run `cancelled`. */
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

  /** runId → the execution this process is running for it. */
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
   * Wake the claim loop for a run that is ALREADY persisted.
   *
   * This runner does not create the run row: the host wrote it (queued) in the
   * same transaction as the messages it belongs to — that transaction is what
   * makes "the user hit send" survive a crash, and re-deriving the row here
   * would be a second, weaker source of truth. So `enqueue` validates and pokes.
   *
   * Idempotent per the port contract: a re-delivered enqueue for a run that is
   * no longer `queued` is a silent no-op, never a second execution of one turn.
   * `priority` / `availableAt` on the input are NOT re-applied — the store owns
   * them, and the port has no `queued → queued` patch to write them through.
   */
  async enqueue(input: EnqueueInput): Promise<void> {
    const run = await this.store.runs.getRun(input.runId);
    if (!run) {
      throw new RecordNotFoundError(`Run not found: ${input.runId}`, {
        runId: input.runId,
      });
    }
    if (run.scopeId !== input.scopeId) {
      // The stored scope wins: it is what `claimNext` serializes on.
      this.logger?.warn("enqueue scopeId disagrees with the stored run", {
        runId: input.runId,
        enqueued: input.scopeId,
        stored: run.scopeId,
      });
    }
    if (run.status !== "queued") {
      this.logger?.debug("enqueue ignored: run is not queued", {
        runId: run.runId,
        status: run.status,
      });
      return;
    }
    // Record the queue position ONLY when something is already executing in the
    // scope. Marking a free scope busy for a run nobody has claimed yet would
    // make `claimNext` skip the very run we are trying to start.
    if (this.scopeLock.hasActive(run.scopeId)) {
      this.scopeLock.tryAcquire(run.scopeId, run.runId);
    }
    this.kick();
  }

  /**
   * Queued run: cancelled outright, before any worker sees it. Running run: the
   * execution's signal is aborted and the worker decides how to land (TurnRunner
   * emits `run.cancelled` and finalizes the run itself; this runner then sees a
   * terminal run and writes nothing more).
   *
   * A running run with no local execution means another process owns it — see
   * the class doc on the single-process limit. `recover()` reconciles it once
   * that owner's lease expires.
   */
  async requestCancel(runId: string): Promise<void> {
    const run = await this.store.runs.getRun(runId);
    if (!run) return;

    if (run.status === "queued" || run.status === "waiting_approval") {
      // `waiting_approval` is included because the transition table allows it
      // and nothing else in this runner can reach it: a run parked on a human
      // is exactly the kind a user cancels.
      try {
        await this.store.runs.transitionRun(runId, [run.status], "cancelled", {
          finishedAt: this.clock.nowIso(),
        });
        // Only after the store agreed: if a claim beat us to it, this run now
        // HOLDS the scope, and clearing the lock would let a second run in the
        // same scope start alongside it.
        this.scopeLock.remove(run.scopeId, runId);
      } catch (err) {
        // Lost the race: a claim flipped it to `running` between the read and
        // the write. Fall through to the abort path rather than reporting a
        // cancel that did not happen.
        this.logger?.debug("cancel raced a claim; aborting instead", {
          runId,
          error: errorMessage(err),
        });
        this.abortActive(runId);
      }
      return;
    }

    if (run.status === "running") {
      if (!this.abortActive(runId)) {
        this.logger?.info(
          "cancel for a running run with no local execution; recover() will reconcile",
          { runId },
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
   * succeeded), and its run, still `running`, either gets a fresh attempt or is
   * dead-lettered for having burned its budget.
   *
   * Safe to call before `startWorker`; with no worker started there is nobody to
   * hand the work to, so abandoned runs are left for the next owner's `recover`
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
    const expired = await this.store.runs.expireStaleLeases(this.clock.now());
    const report: RecoveryReport = {
      expired: expired.length,
      redispatched: 0,
      deadLettered: 0,
    };
    const worker = this.worker;

    for (const lease of expired) {
      const run = await this.store.runs.getRun(lease.runId);
      if (!run) continue;

      // A terminal run means its worker finalized the attempt before dying (or
      // the lease simply outlived a clean finish). Its recorded outcome is the
      // truth; overwriting it with `abandoned` would be a lie about what
      // happened.
      if (run.status !== "running") continue;

      try {
        await this.store.runs.endAttempt({
          attemptId: lease.attemptId,
          status: "abandoned",
          error: "lease expired",
        });
      } catch (err) {
        this.logger?.warn("could not end an abandoned attempt", {
          runId: run.runId,
          attemptId: lease.attemptId,
          error: errorMessage(err),
        });
      }

      if (run.attemptCount >= this.maxAttempts) {
        await this.deadLetter(run.runId, POISON_REASON);
        report.deadLettered += 1;
        continue;
      }

      if (!worker || this.stopped) {
        this.logger?.info("abandoned run left for the next owner to recover", {
          runId: run.runId,
          attempts: run.attemptCount,
        });
        continue;
      }

      const next = await this.startAttempt(run.runId);
      // The re-dispatch continues the SAME run: same id, same event sequence,
      // one more attempt.
      this.dispatch({ run, attempt: next.attempt, lease: next.lease }, worker);
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
   * the slowest run, which is precisely the task-system bug this replaces.
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
        let claimed: ClaimedRun | null;
        try {
          claimed = await this.store.runs.claimNext({
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
   * on the very next iteration already accounts for this run.
   */
  private dispatch(claimed: ClaimedRun, worker: TaskWorker): void {
    const { run } = claimed;
    const superseded = this.active.get(run.runId);
    if (superseded) {
      // Only reachable via `recover()` re-dispatching a run whose previous
      // execution is still hanging in this process. The new one owns the run
      // from here; the old one is fenced out by the lease and will write
      // nothing when it eventually settles.
      this.logger?.warn("superseding a still-running execution after recovery", {
        runId: run.runId,
      });
    }
    if (!this.scopeLock.tryAcquire(run.scopeId, run.runId)) {
      // Should not happen: the claim passed `scopesBusy` from this same lock.
      // The store already handed us the lease, so refusing to run would strand
      // the run in `running` with nobody executing it — log and proceed.
      this.logger?.warn("claimed a run whose scope was locally busy", {
        runId: run.runId,
        scopeId: run.scopeId,
        holder: this.scopeLock.activeRun(run.scopeId),
      });
    }
    const entry: ActiveExecution = {
      runId: run.runId,
      scopeId: run.scopeId,
      controller: new AbortController(),
      cancelRequested: false,
    };
    this.active.set(run.runId, entry);

    const promise = this.executeClaimed(claimed, worker, entry);
    this.inFlight.add(promise);
    void promise.finally(() => {
      this.inFlight.delete(promise);
    });
  }

  /**
   * Drive one run to a terminal state, attempt after attempt.
   *
   * The retry loop lives HERE rather than going back through the claim loop, and
   * holds the scope lock and its concurrency slot for the whole chain: the run
   * never returns to `queued`, so there is nothing for a claim to pick up. See
   * the class doc's point 3.
   */
  private async executeClaimed(
    claimed: ClaimedRun,
    worker: TaskWorker,
    entry: ActiveExecution,
  ): Promise<void> {
    const { runId, scopeId } = entry;
    let attemptId = claimed.attempt.attemptId;
    let lease = claimed.lease;
    try {
      for (;;) {
        const outcome = await this.runAttempt(
          runId,
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
      // Bookkeeping itself failed (the store threw while landing the run). The
      // run stays `running` with a live lease, which is recoverable: the lease
      // expires and `recover()` picks it up. Losing the loop would not be.
      this.logger?.error("task execution bookkeeping failed", {
        runId,
        attemptId,
        error: errorMessage(err),
      });
    } finally {
      // Best effort: the token may already have been replaced (a retry) or
      // expired (a takeover), and both throw here.
      try {
        await this.store.runs.releaseLease(lease.leaseToken);
      } catch {
        /* not ours any more — nothing to release */
      }
      // A superseded execution (recovery re-dispatched its run while it hung)
      // must not release resources the new execution now owns.
      if (this.active.get(runId) === entry) {
        this.active.delete(runId);
        const successor = this.scopeLock.release(scopeId, runId);
        if (successor !== null) {
          this.logger?.debug("scope freed for the next waiter", {
            scopeId,
            runId: successor,
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
   * lost its lease while running is a zombie — some other owner has the run now,
   * and a write from here would interleave with theirs or overwrite a verdict
   * they already recorded.
   */
  private async runAttempt(
    runId: string,
    attemptId: string,
    lease: Lease,
    entry: ActiveExecution,
    worker: TaskWorker,
  ): Promise<AttemptOutcome> {
    entry.controller = new AbortController();
    if (entry.cancelRequested) entry.controller.abort();

    const heartbeat = setInterval(() => {
      void this.renewLease(runId, lease.leaseToken, entry);
    }, this.heartbeatMs);
    (heartbeat as unknown as { unref?: () => void }).unref?.();

    let thrown: unknown;
    let threw = false;
    try {
      await worker.execute({
        runId,
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
        runId,
        attemptId,
      });
      return { kind: "done" };
    }

    return threw
      ? await this.settleThrown(runId, attemptId, entry, thrown)
      : await this.settleResolved(runId, attemptId, entry);
  }

  /**
   * The worker returned. Normally it has already finalized the run itself
   * (TurnRunner transitions the run and ends its own attempt on its way out), in
   * which case there is nothing left to do — the run is no longer `running` and
   * this method writes nothing.
   *
   * The defensive branch below is for a worker that returns without landing the
   * run: leaving it `running` forever with no lease would make it invisible to
   * both the claim loop (which only takes `queued`) and `recover()` (which only
   * sees expired leases).
   */
  private async settleResolved(
    runId: string,
    attemptId: string,
    entry: ActiveExecution,
  ): Promise<AttemptOutcome> {
    const run = await this.store.runs.getRun(runId);
    if (!run || run.status !== "running") return { kind: "done" };
    // A cancel that the worker swallowed still means cancelled, not completed.
    const status = entry.cancelRequested ? "cancelled" : "completed";
    await this.store.runs.endAttempt({ attemptId, status });
    await this.store.runs.transitionRun(runId, ["running"], status, {
      finishedAt: this.clock.nowIso(),
    });
    return { kind: "done" };
  }

  /** The worker threw: classify, then retry / dead-letter / fail / cancel. */
  private async settleThrown(
    runId: string,
    attemptId: string,
    entry: ActiveExecution,
    err: unknown,
  ): Promise<AttemptOutcome> {
    const classified = classifyExecutionError(err);
    const kind = entry.cancelRequested ? "cancelled" : classified.kind;
    const message = errorMessage(err);

    if (kind === "cancelled") {
      await this.store.runs.endAttempt({
        attemptId,
        status: "cancelled",
        error: message,
      });
      await this.landIfRunning(runId, "cancelled");
      return { kind: "done" };
    }

    await this.store.runs.endAttempt({
      attemptId,
      status: "failed",
      error: `${classified.reason}: ${message}`,
    });

    if (kind === "terminal") {
      // NOT dead-lettered: the dead-letter row means "this poisoned the queue,
      // stop feeding it work". A cleanly-diagnosed failure is just a failure.
      this.logger?.info("run failed with a terminal error", {
        runId,
        attemptId,
        reason: classified.reason,
      });
      await this.landIfRunning(runId, "failed", `${classified.reason}: ${message}`);
      return { kind: "done" };
    }

    // Transient from here down.
    const run = await this.store.runs.getRun(runId);
    if (!run || run.status !== "running") {
      // A worker that lands the run itself before rethrowing (TurnRunner's
      // `failQuietly` does exactly this) has already spent the run's life; the
      // transition table has no way back from `failed`, and inventing one to
      // force a retry would resurrect a run the worker deliberately buried.
      // Such a worker opts out of runner-level retries by construction — its
      // own retry policy lives inside the turn.
      this.logger?.debug("no retry: the worker already landed the run", {
        runId,
        status: run?.status,
      });
      return { kind: "done" };
    }

    // `attemptCount` is the store's own count, incremented by `createAttempt` —
    // no separate bookkeeping to drift out of sync with the attempt rows.
    if (run.attemptCount >= this.maxAttempts) {
      await this.deadLetter(runId, `${classified.reason}: ${message}`);
      return { kind: "done" };
    }
    if (this.stopped || !this.worker) {
      // Shutting down mid-retry: leave the run `running` with a live lease that
      // will expire, so the next owner's `recover()` continues it.
      this.logger?.info("retry deferred to recovery: the worker is stopping", {
        runId,
        attempts: run.attemptCount,
      });
      return { kind: "done" };
    }

    this.logger?.info("retrying run in place", {
      runId,
      attempt: run.attemptCount + 1,
      reason: classified.reason,
    });
    const next = await this.startAttempt(runId);
    return {
      kind: "retry",
      next: { attemptId: next.attempt.attemptId, lease: next.lease },
    };
  }

  // ────────────────────────────── store ops ──────────────────────────────

  /**
   * A fresh attempt + lease on a run that stays `running`.
   *
   * Acquiring replaces the run's lease and draws the next fencing token, so the
   * previous attempt's `leaseToken` stops being accepted by `appendEvents` from
   * this moment — that is what stops a zombie from interleaving its events into
   * the live attempt's stream.
   */
  private async startAttempt(
    runId: string,
  ): Promise<{ attempt: AttemptRecord; lease: Lease }> {
    const attempt = await this.store.runs.createAttempt({
      attemptId: `att_${crypto.randomUUID()}`,
      runId,
      ownerId: this.ownerId,
    });
    const lease = await this.store.runs.acquireLease({
      runId,
      attemptId: attempt.attemptId,
      ownerId: this.ownerId,
      ttlMs: this.leaseTtlMs,
    });
    return { attempt, lease };
  }

  /** Mark the run poisoned AND land it failed — the row alone stops nothing. */
  private async deadLetter(runId: string, reason: string): Promise<void> {
    this.logger?.warn("dead-lettering run", { runId, reason });
    await this.store.runs.markDeadLettered(runId, reason);
    await this.landIfRunning(runId, "failed", reason);
  }

  /**
   * Transition a run that is still `running`, tolerating the race where it is
   * not: the worker may have finalized it (TurnRunner does), or a cancel may
   * have landed first. Re-reading rather than assuming is what keeps the runner
   * from throwing `InvalidRunTransitionError` at itself on every happy path.
   */
  private async landIfRunning(
    runId: string,
    to: "completed" | "failed" | "cancelled",
    error?: string,
  ): Promise<void> {
    const run = await this.store.runs.getRun(runId);
    if (!run || run.status !== "running") {
      this.logger?.debug("run already terminal; skipping transition", {
        runId,
        status: run?.status,
        intended: to,
      });
      return;
    }
    await this.store.runs.transitionRun(runId, ["running"], to, {
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
      await this.store.runs.renewLease(leaseToken, this.leaseTtlMs);
      return true;
    } catch (err) {
      if (err instanceof LeaseLostError) return false;
      throw err;
    }
  }

  private async renewLease(
    runId: string,
    leaseToken: string,
    entry: ActiveExecution,
  ): Promise<void> {
    try {
      await this.store.runs.renewLease(leaseToken, this.leaseTtlMs);
    } catch (err) {
      if (err instanceof LeaseLostError) {
        // The run belongs to someone else now. Abort so the worker stops
        // burning provider calls on output nobody will accept — and so the
        // fencing check after `execute` has an aborted attempt to report on.
        entry.controller.abort();
        this.logger?.warn("lease lost mid-attempt; aborting the execution", {
          runId,
          error: errorMessage(err),
        });
        return;
      }
      // A store hiccup is not a lost lease. Let the next heartbeat try again
      // rather than killing a healthy attempt over one failed write.
      this.logger?.warn("lease renewal failed; will retry on the next beat", {
        runId,
        error: errorMessage(err),
      });
    }
  }

  /** Abort the local execution of `runId`. False when there is none. */
  private abortActive(runId: string): boolean {
    const entry = this.active.get(runId);
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
