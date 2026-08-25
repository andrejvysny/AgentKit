/**
 * A seeded, deterministic, randomized schedule over a {@link TaskStore} — the
 * driver half of the durability suite whose grading half is
 * `task-invariants.ts`.
 *
 * WHY RANDOM AT ALL. Every hand-written concurrency test encodes the
 * interleaving its author already suspected. The failures that actually reach
 * production are the ones nobody wrote down: a cancel that lands between the
 * claim and the first event, a lease that expires while its worker is mid-write,
 * a retry that starts on a task another worker just settled through the
 * dependency gate. A driver that draws its schedule from a seeded PRNG explores
 * those without anyone naming them, and a fixed seed set makes the exploration
 * a regression test rather than a lottery.
 *
 * WHY DETERMINISTIC, AND HOW. Three rules, all load-bearing:
 *
 * 1. EVERY WORKER HAS ITS OWN PRNG, seeded from the run seed and its index. A
 *    single shared generator would make each worker's decisions a function of
 *    the interleaving, so the same seed would explore a different schedule the
 *    moment anything's timing shifted — the opposite of reproducible.
 * 2. TIME IS LOGICAL. The clock is injected and only moves when the driver says
 *    so, so lease expiry is a decision rather than a race with the wall clock.
 *    Nothing here sleeps; the suite is bounded by work, not by seconds.
 * 3. NOTHING TOUCHES A TIMER. Interleaving comes from `await` alone, which the
 *    microtask queue orders deterministically for a fixed program.
 *
 * WHAT IT MODELS. The worker loop is the one
 * `SingleProcessTaskRunner` runs, reduced to its store calls: claim, write
 * events under the lease, land the task, end the attempt, release. Retry is
 * IN PLACE (a new attempt on a task that stays `running`) because
 * `TASK_TRANSITIONS` has no `running → queued` edge; recovery is
 * `expireStaleLeases` followed by ending the abandoned attempt and either
 * re-attempting or dead-lettering. Getting those two right is the point: they
 * are where the fencing token and the unbroken event sequence earn their keep.
 *
 * NO RUNTIME `@agentkit/host` DEPENDENCY, same rule as `store-conformance.ts`.
 * `TaskService` is reached through injected callbacks
 * ({@link TaskScheduleTarget.submitTask} / {@link TaskScheduleTarget.cancelTask})
 * rather than imported, and errors are matched by their `code` string rather
 * than by `instanceof`.
 */
import { CONTRACT_VERSION, type TaskEventEnvelope } from "@agentkit/contracts";
import type {
  AssistantStore,
  AttemptRecord,
  ClaimedTask,
  Clock,
  IdGenerator,
  Lease,
  TaskRecord,
} from "@agentkit/host";
import {
  checkTaskInvariants,
  snapshotTaskInvariants,
  type ObservedLease,
  type TaskInvariantView,
} from "./task-invariants.js";

// ---------------------------------------------------------------------------
// Seeded randomness and logical time
// ---------------------------------------------------------------------------

/**
 * mulberry32 — 32-bit state, one multiply-xorshift round, uniform enough for
 * schedule selection and short enough to inline. Written out here rather than
 * taken from a package because this workspace adds no dependency for eleven
 * lines, and a PRNG a test depends on for reproducibility should be a thing you
 * can read.
 */
export function mulberry32(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** The draws a schedule needs, over one {@link mulberry32} stream. */
export interface Rng {
  /** Uniform in [0, 1). */
  next(): number;
  /** Uniform integer in [0, maxExclusive). */
  int(maxExclusive: number): number;
  /** True with probability `p`. */
  chance(p: number): boolean;
  /** One of `items`; throws on an empty list rather than returning undefined. */
  pick<T>(items: readonly T[]): T;
}

export function createRng(seed: number): Rng {
  const next = mulberry32(seed);
  return {
    next,
    int: (maxExclusive: number) => Math.floor(next() * maxExclusive),
    chance: (p: number) => next() < p,
    pick: <T>(items: readonly T[]): T => {
      if (items.length === 0) throw new Error("Rng.pick on an empty list");
      return items[Math.floor(next() * items.length)]!;
    },
  };
}

/** A {@link Clock} the driver moves by hand — see rule 2 in the module doc. */
export interface LogicalClock extends Clock {
  advance(ms: number): void;
}

export function createLogicalClock(
  startIso = "2026-01-01T00:00:00.000Z",
): LogicalClock {
  let current = new Date(startIso).getTime();
  return {
    now: () => new Date(current),
    nowIso: () => new Date(current).toISOString(),
    advance: (ms: number) => {
      current += ms;
    },
  };
}

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/** What {@link TaskScheduleTarget.submitTask} is asked to persist. */
export interface ScheduleTaskSpec {
  taskId: string;
  kind: string;
  scopeId: string;
  payload: Record<string, unknown>;
  priority?: number;
  availableAt?: string;
  parentTaskId?: string;
  dependsOn?: string[];
}

/**
 * The store under test, plus the two host-level entry points the driver reaches
 * through callbacks instead of importing (see the module doc).
 */
export interface TaskScheduleTarget {
  /**
   * One or more handles onto the SAME durable state. Worker `i` uses
   * `handles[i % handles.length]`, so a single-element list is the ordinary
   * one-process topology and a two-element list is two connections racing over
   * one database file.
   */
  handles: readonly AssistantStore[];
  /** Wire `TaskService.submitTask`. */
  submitTask(spec: ScheduleTaskSpec): Promise<TaskRecord>;
  /** Wire `TaskService.cancelTask` — the cascading, lineage-following one. */
  cancelTask(taskId: string): Promise<void>;
  /**
   * True once `TaskRunner.requestCancel` has been called for this task. A
   * running task is cancelled COOPERATIVELY: the service asks, and the worker
   * (here, {@link runAttempt}) honours it by landing the task itself.
   */
  cancelRequested(taskId: string): boolean;
  /** The adapter's attempt table, read through its own handle. See task-invariants.ts. */
  dumpAttempts?(): readonly AttemptRecord[];
  /** The adapter's live lease rows, read through its own handle. */
  dumpLiveLeases?(): readonly Lease[];
}

export interface TaskScheduleOptions {
  target: TaskScheduleTarget;
  seed: number;
  clock: LogicalClock;
  ids: IdGenerator;
  /** Must match what the stores were constructed with — the driver reasons about expiry. */
  leaseTtlMs: number;
  /** Concurrent in-process workers. Default 3. */
  workers?: number;
  /** Tasks seeded before the workers start. Default 24. */
  tasks?: number;
  /** Scheduler steps each worker takes before the drain phase. Default 40. */
  steps?: number;
  /** Attempts a task gets before it is dead-lettered. Default 3. */
  maxAttempts?: number;
  /** Every Nth step, a worker spot-checks the invariants mid-run. Default 7. */
  spotCheckEvery?: number;
  /** Task kind for everything this run creates. */
  kind?: string;
}

export interface TaskScheduleStats {
  created: number;
  claimed: number;
  completed: number;
  failed: number;
  cancelled: number;
  deadLettered: number;
  retries: number;
  crashes: number;
  recovered: number;
  leaseLosses: number;
}

export interface TaskScheduleResult {
  seed: number;
  /** The snapshot to hand {@link checkTaskInvariants} — taken at quiescence. */
  view: TaskInvariantView;
  /** Violations any mid-run spot check found, already labelled with the step. */
  spotCheckViolations: readonly string[];
  /** Tasks still non-terminal after the drain phase. Empty on a healthy run. */
  undrained: readonly string[];
  stats: TaskScheduleStats;
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/** `code` on an AgentKitHostError, without importing the class. See the module doc. */
function errorCode(err: unknown): string | undefined {
  return typeof err === "object" && err !== null
    ? (err as { code?: string }).code
    : undefined;
}

/** How one attempt at a task behaves, drawn from the worker's own stream. */
type AttemptOutcome =
  /** Land the task `completed` — the happy path. */
  | "complete"
  /** Land it `failed`: a terminal error the runner would not retry. */
  | "fail"
  /** End the attempt `failed` and try again IN PLACE on the same running task. */
  | "retry"
  /** Walk away holding the lease: the process that died mid-attempt. */
  | "crash"
  /** Park on `waiting_approval` and come straight back, then complete. */
  | "park";

export async function runTaskSchedule(
  options: TaskScheduleOptions,
): Promise<TaskScheduleResult> {
  const {
    target,
    seed,
    clock,
    ids,
    leaseTtlMs,
    workers = 3,
    tasks = 24,
    steps = 40,
    maxAttempts = 3,
    spotCheckEvery = 7,
    kind = "durability.unit",
  } = options;

  const first = target.handles[0];
  if (first === undefined) throw new Error("TaskScheduleTarget needs a handle");
  /** The handle the driver reads snapshots and drains through. */
  const primary: AssistantStore = first;

  const taskIds: string[] = [];
  const observedLeases: ObservedLease[] = [];
  const busyScopes = new Set<string>();
  const inFlight = new Set<string>();
  /**
   * Workers inside a claim or a recovery right now — the guard on moving the
   * clock, and NOT the same thing as `inFlight`.
   *
   * A lease is minted inside `claimNext`, several awaits before the caller gets
   * to record the task as in flight. Gate the clock on `inFlight` alone and
   * another worker can jump past the TTL inside that window, so the lease is
   * born already expired and its perfectly healthy owner becomes a zombie. This
   * counter is raised BEFORE the claim and lowered after the attempt lands,
   * which closes that window.
   */
  let busyWorkers = 0;
  const spotCheckViolations: string[] = [];
  const stats: TaskScheduleStats = {
    created: 0,
    claimed: 0,
    completed: 0,
    failed: 0,
    cancelled: 0,
    deadLettered: 0,
    retries: 0,
    crashes: 0,
    recovered: 0,
    leaseLosses: 0,
  };
  let leaseCounter = 0;

  const observe = (lease: Lease): void => {
    leaseCounter += 1;
    observedLeases.push({ ...lease, observedAt: leaseCounter });
  };

  // ── workload ─────────────────────────────────────────────────────────────
  // Built and persisted up front, in order: `dependsOn` may only name rows that
  // already exist (that rule is what makes the graph acyclic by construction),
  // so the generator can only ever point backwards.
  const seedRng = createRng(seed ^ 0x9e3779b9);
  const scopes: string[] = [];
  for (let i = 0; i < tasks; i += 1) {
    const taskId = `t${String(i).padStart(3, "0")}`;
    const scopeId =
      scopes.length > 0 && seedRng.chance(0.3)
        ? seedRng.pick(scopes)
        : `scope-${i}`;
    if (!scopes.includes(scopeId)) scopes.push(scopeId);
    const spec: ScheduleTaskSpec = {
      taskId,
      kind,
      scopeId,
      payload: { index: i },
      priority: seedRng.int(4),
    };
    const shape = seedRng.next();
    if (taskIds.length > 0 && shape < 0.2) {
      // Chain: one edge to the task created just before this one.
      spec.dependsOn = [taskIds[taskIds.length - 1]!];
    } else if (taskIds.length > 2 && shape < 0.35) {
      // Fan-in: two distinct earlier tasks must both complete first.
      const a = seedRng.pick(taskIds);
      const b = seedRng.pick(taskIds);
      spec.dependsOn = a === b ? [a] : [a, b];
    } else if (taskIds.length > 0 && shape < 0.45) {
      // Lineage, NOT a gate: a child runs whether or not its parent has.
      spec.parentTaskId = seedRng.pick(taskIds);
    } else if (shape < 0.52) {
      // Not claimable yet; the drain phase's clock jump releases it.
      spec.availableAt = new Date(clock.now().getTime() + 250).toISOString();
    }
    await target.submitTask(spec);
    taskIds.push(taskId);
    stats.created += 1;
  }

  // ── one attempt ──────────────────────────────────────────────────────────

  /** Write 1–3 events under the lease, continuing the task's ONE sequence. */
  async function emitEvents(
    store: AssistantStore,
    claim: { taskId: string; attemptId: string; leaseToken: string },
    rng: Rng,
    marker: string,
  ): Promise<boolean> {
    const count = 1 + rng.int(3);
    // `nextSeq` is what makes a retry continue the sequence instead of
    // restarting it — attempt 2 picks up where attempt 1 stopped.
    const first = await store.tasks.nextSeq(claim.taskId);
    const events: TaskEventEnvelope[] = [];
    for (let i = 0; i < count; i += 1) {
      events.push({
        type: `durability.${marker}`,
        seq: first + i,
        eventId: ids.eventId(),
        timestamp: clock.nowIso(),
        contractVersion: CONTRACT_VERSION,
        attemptId: claim.attemptId,
      });
    }
    try {
      await store.tasks.appendEvents(claim.taskId, events, {
        leaseToken: claim.leaseToken,
      });
      return true;
    } catch (err) {
      // The lease was taken over (recovery expired it) while this attempt was
      // mid-write. The attempt is no longer this worker's to land: whoever
      // holds the lease now owns the outcome, so it writes nothing further.
      if (errorCode(err) === "lease_lost") {
        stats.leaseLosses += 1;
        return false;
      }
      throw err;
    }
  }

  /** Land a task, tolerating the cancel/finish race the runner tolerates. */
  async function land(
    store: AssistantStore,
    taskId: string,
    to: "completed" | "failed" | "cancelled",
    error?: string,
  ): Promise<boolean> {
    try {
      await store.tasks.transitionTask(taskId, ["running"], to, {
        finishedAt: clock.nowIso(),
        ...(error === undefined ? {} : { error }),
      });
      return true;
    } catch (err) {
      // Someone else landed it first (a cancel, a recovery dead-letter). That
      // outcome is the truer one — see `SingleProcessTaskRunner.landIfRunning`.
      if (errorCode(err) === "invalid_task_transition") return false;
      throw err;
    }
  }

  /**
   * Execute one claimed attempt, retrying IN PLACE up to `maxAttempts` and
   * dead-lettering at the cap — the runner's policy, reduced to store calls.
   */
  async function runAttempt(
    store: AssistantStore,
    claimed: ClaimedTask,
    rng: Rng,
    forceComplete: boolean,
  ): Promise<void> {
    let attemptId = claimed.attempt.attemptId;
    let leaseToken = claimed.lease.leaseToken;
    const taskId = claimed.task.taskId;

    for (;;) {
      const outcome: AttemptOutcome = forceComplete
        ? "complete"
        : pickOutcome(rng);

      const wrote = await emitEvents(
        store,
        { taskId, attemptId, leaseToken },
        rng,
        outcome,
      );
      if (!wrote) return; // fenced out mid-attempt; not ours to land.

      // A cancel that arrived while this attempt was running beats whatever
      // the schedule drew: cancellation of running work is cooperative, and
      // this is the worker honouring it.
      if (target.cancelRequested(taskId)) {
        await land(store, taskId, "cancelled");
        await endAttempt(store, attemptId, "cancelled");
        await release(store, leaseToken);
        stats.cancelled += 1;
        return;
      }

      if (outcome === "crash") {
        // No endAttempt, no release: the lease stays live and expires later,
        // which is the ONLY evidence a crash leaves behind.
        stats.crashes += 1;
        return;
      }

      if (outcome === "park") {
        // `waiting_approval` and straight back. Parked-and-then-crashed is
        // deliberately not modelled: recovery only resumes `running` tasks, so
        // a task abandoned in `waiting_approval` would never drain and the
        // driver would be asserting its own bug.
        const parked = await park(store, taskId);
        if (!parked) {
          await endAttempt(store, attemptId, "failed", "lost the park race");
          await release(store, leaseToken);
          return;
        }
      }

      if (outcome === "retry") {
        const task = await store.tasks.getTask(taskId);
        if (!task || task.status !== "running") {
          await endAttempt(store, attemptId, "failed", "landed elsewhere");
          await release(store, leaseToken);
          return;
        }
        await endAttempt(store, attemptId, "failed", "transient");
        if (task.attemptCount >= maxAttempts) {
          await store.tasks.markDeadLettered(taskId, "poison: retry budget");
          await land(store, taskId, "failed", "poison: retry budget");
          await release(store, leaseToken);
          stats.deadLettered += 1;
          return;
        }
        // In place: a NEW attempt and a NEW lease on a task that never left
        // `running`. The fresh lease fences the previous attempt's token out.
        const next = await startAttempt(store, taskId, claimed.lease.ownerId);
        if (next === null) return;
        attemptId = next.attempt.attemptId;
        leaseToken = next.lease.leaseToken;
        stats.retries += 1;
        continue;
      }

      const to = outcome === "fail" ? "failed" : "completed";
      const landed = await land(
        store,
        taskId,
        to,
        to === "failed" ? "executor said no" : undefined,
      );
      await endAttempt(
        store,
        attemptId,
        landed ? (to === "failed" ? "failed" : "completed") : "cancelled",
      );
      await release(store, leaseToken);
      if (landed) {
        if (to === "failed") stats.failed += 1;
        else stats.completed += 1;
      }
      return;
    }
  }

  function pickOutcome(rng: Rng): AttemptOutcome {
    const roll = rng.next();
    if (roll < 0.55) return "complete";
    if (roll < 0.68) return "retry";
    if (roll < 0.78) return "fail";
    if (roll < 0.9) return "crash";
    return "park";
  }

  async function park(store: AssistantStore, taskId: string): Promise<boolean> {
    try {
      await store.tasks.transitionTask(taskId, ["running"], "waiting_approval");
      await store.tasks.transitionTask(taskId, ["waiting_approval"], "running");
      return true;
    } catch (err) {
      if (errorCode(err) === "invalid_task_transition") return false;
      throw err;
    }
  }

  async function endAttempt(
    store: AssistantStore,
    attemptId: string,
    status: "completed" | "failed" | "cancelled" | "abandoned",
    error?: string,
  ): Promise<void> {
    try {
      await store.tasks.endAttempt({
        attemptId,
        status,
        ...(error === undefined ? {} : { error }),
      });
    } catch (err) {
      // Recovery ended it first — that is the abandoned outcome standing.
      if (errorCode(err) !== "not_found") throw err;
    }
  }

  async function release(
    store: AssistantStore,
    leaseToken: string,
  ): Promise<void> {
    try {
      await store.tasks.releaseLease(leaseToken);
    } catch (err) {
      // Already expired or replaced; there is nothing left to give back.
      if (errorCode(err) !== "lease_lost") throw err;
    }
  }

  async function startAttempt(
    store: AssistantStore,
    taskId: string,
    ownerId: string,
  ): Promise<{ attempt: AttemptRecord; lease: Lease } | null> {
    const attempt = await store.tasks.createAttempt({
      attemptId: ids.attemptId(),
      taskId,
      ownerId,
    });
    const lease = await store.tasks.acquireLease({
      taskId,
      attemptId: attempt.attemptId,
      ownerId,
      ttlMs: leaseTtlMs,
    });
    observe(lease);
    return { attempt, lease };
  }

  // ── worker actions ───────────────────────────────────────────────────────

  async function claimAndRun(
    store: AssistantStore,
    ownerId: string,
    rng: Rng,
    forceComplete: boolean,
  ): Promise<boolean> {
    busyWorkers += 1;
    try {
      const claimed = await store.tasks.claimNext({
        ownerId,
        now: clock.now(),
        scopesBusy: [...busyScopes],
      });
      if (claimed === null) return false;
      stats.claimed += 1;
      observe(claimed.lease);
      busyScopes.add(claimed.task.scopeId);
      inFlight.add(claimed.task.taskId);
      try {
        await runAttempt(store, claimed, rng, forceComplete);
      } finally {
        busyScopes.delete(claimed.task.scopeId);
        inFlight.delete(claimed.task.taskId);
      }
      return true;
    } finally {
      busyWorkers -= 1;
    }
  }

  /**
   * `SingleProcessTaskRunner.recoverWithReport`, reduced to store calls: the
   * expired lease is the only evidence a worker died, its attempt ends
   * `abandoned` (nobody knows whether the work succeeded), and the task —
   * still `running`, because there is no way back to `queued` — either gets a
   * fresh attempt or burns the last of its budget.
   */
  async function recover(
    store: AssistantStore,
    ownerId: string,
    rng: Rng,
    forceComplete: boolean,
  ): Promise<void> {
    busyWorkers += 1;
    try {
      await recoverExclusive(store, ownerId, rng, forceComplete);
    } finally {
      busyWorkers -= 1;
    }
  }

  async function recoverExclusive(
    store: AssistantStore,
    ownerId: string,
    rng: Rng,
    forceComplete: boolean,
  ): Promise<void> {
    const expired = await store.tasks.expireStaleLeases(clock.now());
    for (const lease of expired) {
      const task = await store.tasks.getTask(lease.taskId);
      if (!task || task.status !== "running") continue;
      await endAttempt(store, lease.attemptId, "abandoned", "lease expired");
      stats.recovered += 1;
      if (task.attemptCount >= maxAttempts) {
        await store.tasks.markDeadLettered(task.taskId, "poison: abandoned");
        await land(store, task.taskId, "failed", "poison: abandoned");
        stats.deadLettered += 1;
        continue;
      }
      const next = await startAttempt(store, task.taskId, ownerId);
      if (next === null) continue;
      busyScopes.add(task.scopeId);
      inFlight.add(task.taskId);
      try {
        await runAttempt(
          store,
          { task, attempt: next.attempt, lease: next.lease },
          rng,
          forceComplete,
        );
      } finally {
        busyScopes.delete(task.scopeId);
        inFlight.delete(task.taskId);
      }
    }
  }

  async function spotCheck(label: string): Promise<void> {
    const view = await snapshot();
    spotCheckViolations.push(
      ...checkTaskInvariants(view, { phase: "in-flight", label }),
    );
  }

  // ── the schedule ─────────────────────────────────────────────────────────

  async function worker(index: number): Promise<void> {
    const rng = createRng((seed + index * 0x85ebca6b) | 0);
    const store = target.handles[index % target.handles.length]!;
    const ownerId = `worker-${index}`;
    for (let step = 0; step < steps; step += 1) {
      const roll = rng.next();
      if (roll < 0.68) {
        await claimAndRun(store, ownerId, rng, false);
      } else if (roll < 0.78) {
        // A cancel aimed at whatever is around — queued, running, or already
        // terminal. The cascade follows lineage, so this also cancels branches.
        await target.cancelTask(rng.pick(taskIds));
      } else if (roll < 0.9) {
        // Jump past the TTL so a lease left behind by a crash goes stale, then
        // recover it.
        //
        // ONLY WHEN NOBODY IS MID-ATTEMPT, and that condition is the difference
        // between a suite that grades the adapter and one that grades itself.
        // Expiring a LIVE worker's lease makes that worker a zombie, and a
        // zombie's `transitionTask` still lands the task (the port's CAS is on
        // status, not on the lease) — so the driver would be manufacturing the
        // double-writer it is supposed to be watching the store prevent, and
        // every resulting violation would be the driver's own. Crashed tasks
        // are already out of `inFlight`, so this still reaches every lease a
        // crash left behind.
        if (busyWorkers === 0) clock.advance(leaseTtlMs + 1);
        await recover(store, ownerId, rng, false);
      } else if (busyWorkers === 0) {
        // Same rule as the jump above, and for the same reason: time only moves
        // when no lease is live, so a lease can go stale only by being
        // abandoned. Drifting the clock under a working worker would expire its
        // lease by accident and turn it into the zombie writer described above.
        clock.advance(1 + rng.int(20));
      }
      if (step % spotCheckEvery === spotCheckEvery - 1) {
        await spotCheck(`seed ${seed} worker ${index} step ${step}`);
      }
    }
  }

  await Promise.all(
    Array.from({ length: workers }, (_, index) => worker(index)),
  );

  // ── drain to quiescence ──────────────────────────────────────────────────
  // Deterministic from here: every attempt completes, so the only tasks left
  // non-terminal are ones the store will not let anyone finish — which is
  // exactly what `undrained` reports.
  const drainRng = createRng((seed ^ 0xc2b2ae35) | 0);
  for (let round = 0; round < 12; round += 1) {
    clock.advance(leaseTtlMs + 1000);
    await recover(primary, "drain", drainRng, true);
    let guard = 0;
    while (await claimAndRun(primary, "drain", drainRng, true)) {
      if (++guard > 10 * (tasks + 1)) break;
    }
    const remaining = await nonTerminal();
    if (remaining.length === 0) break;
  }

  async function nonTerminal(): Promise<string[]> {
    const out: string[] = [];
    for (const taskId of taskIds) {
      const task = await primary.tasks.getTask(taskId);
      if (
        task &&
        task.status !== "completed" &&
        task.status !== "failed" &&
        task.status !== "cancelled"
      ) {
        out.push(`${taskId}:${task.status}`);
      }
    }
    return out;
  }

  async function snapshot(): Promise<TaskInvariantView> {
    return snapshotTaskInvariants({
      reader: primary.tasks,
      taskIds,
      observedLeases,
      dumpAttempts: target.dumpAttempts?.bind(target),
      dumpLiveLeases: target.dumpLiveLeases?.bind(target),
      inFlightTaskIds: inFlight,
    });
  }

  return {
    seed,
    view: await snapshot(),
    spotCheckViolations,
    undrained: await nonTerminal(),
    stats,
  };
}
