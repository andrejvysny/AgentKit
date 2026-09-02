/**
 * The durable-execution invariants of a {@link TaskStore}, checked against a
 * snapshot instead of against one hand-written scenario.
 *
 * A conformance test says "this call does this thing". This file says something
 * different and stronger: WHATEVER a pile of concurrent workers just did to a
 * store, these statements must still hold. That makes it the grading half of a
 * randomized schedule (`task-schedule-driver.ts`) and of a fault-injection
 * suite — the part that turns "it did not throw" into "the durable state is
 * still a state the port permits".
 *
 * FRAMEWORK-NEUTRAL, like `store-conformance.ts`: no `bun:test`, no `expect`.
 * The checker RETURNS the violations it found (empty array = clean) and the
 * caller asserts on that with whatever runner it has. Every `@agentkit/host`
 * import is `import type` for the same reason as in that file — this package
 * takes host as a peer dependency and must carry no runtime edge to it.
 *
 * ── WHAT IS CHECKABLE FROM OUTSIDE, AND WHAT IS NOT ────────────────────────
 *
 * `TaskStore` exposes `getTask`, `listChildren` and `listEvents`, and nothing
 * else that reads state back. There is no `listAttempts` and no `listLeases`,
 * on purpose: they would exist for tests alone, and a port method that only
 * tests call is a port method that stops being true. So this checker splits its
 * evidence in two:
 *
 * - PUBLIC READS — tasks, their event logs, their children. Always available,
 *   and every invariant derived from them holds for any adapter.
 * - WHAT THE DRIVER OBSERVED — the leases and attempts handed back to the
 *   caller by `claimNext` / `createAttempt` / `acquireLease`. Enough to check
 *   fencing monotonicity and "every attempt I was given is really recorded",
 *   because a token the store never handed out cannot be observed and a token
 *   it did hand out must be honoured.
 * - AN ADAPTER DUMP — the attempt and lease tables read through the adapter's
 *   own handle (the memory store's public `attempts` Map, a second read-only
 *   `bun:sqlite` connection on the same file). Supplied by the TEST LAYER,
 *   which is allowed to know which adapter it is grading; this package is not.
 *   {@link TaskInvariantView.attemptsAreComplete} says whether a dump is what
 *   `attempts` holds, and the checks that need completeness ("this task has at
 *   most one RUNNING attempt", "attempt numbers are 1..n with no holes") run
 *   only when it is set. Without a dump they would be vacuous rather than
 *   wrong: a driver cannot observe an attempt the store invented behind its
 *   back.
 */
import type { TaskEventEnvelope } from "@agentkit/contracts";
import type {
  AttemptRecord,
  Lease,
  TaskRecord,
  TaskStatus,
} from "@agentkit/host";

/** Statuses a task can never leave — see `TASK_TRANSITIONS`. */
const TERMINAL_STATUSES: readonly TaskStatus[] = [
  "completed",
  "failed",
  "cancelled",
];

function isTerminal(status: TaskStatus): boolean {
  return TERMINAL_STATUSES.includes(status);
}

/** A task whose worker holds it: claimed and not yet landed. */
function isHeld(status: TaskStatus): boolean {
  return status === "running" || status === "waiting_approval";
}

/**
 * One lease as the DRIVER saw it come back, plus the order it saw it in.
 *
 * `observedAt` is a driver-local counter, not a timestamp: fencing monotonicity
 * is a statement about the order leases were ISSUED, and wall-clock stamps
 * collide at this granularity. Two leases on one task can never be issued
 * concurrently — the second needs the first attempt to have ended — so the
 * order the driver appended them in is the order the store issued them in.
 */
export interface ObservedLease extends Lease {
  observedAt: number;
}

/** A full store dump as the driver can assemble it. See the module doc. */
export interface TaskInvariantView {
  /**
   * Every task under test, re-read through `getTask` at snapshot time.
   *
   * ORDER MATTERS FOR A MID-RUN VIEW: the dependency check is only sound when
   * every task was read no EARLIER than the tasks depending on it (read
   * dependents first, dependencies after). A driver assembling an `in-flight`
   * view must read newest-first — see `task-schedule-driver.ts`'s `snapshot`.
   * At quiescence nothing is moving and the order is irrelevant.
   */
  tasks: readonly TaskRecord[];
  /** Complete `listEvents(taskId)` per task — no `afterSeq`, no `limit`. */
  events: ReadonlyMap<string, readonly TaskEventEnvelope[]>;
  /** `listChildren(taskId)` per task: parent id → child ids. */
  children: ReadonlyMap<string, readonly string[]>;
  /** Every lease the driver was handed, in the order it was handed them. */
  observedLeases: readonly ObservedLease[];
  /** Attempt rows: an adapter dump when there is one, driver observations otherwise. */
  attempts: readonly AttemptRecord[];
  /** True when `attempts` is a real adapter dump — see the module doc. */
  attemptsAreComplete: boolean;
  /** Live lease rows from an adapter dump. Absent when the adapter cannot be dumped. */
  liveLeases?: readonly Lease[];
  /** Tasks a worker is executing RIGHT NOW. Empty at quiescence. */
  inFlightTaskIds: ReadonlySet<string>;
}

export type TaskInvariantPhase = "quiescent" | "in-flight";

export interface CheckTaskInvariantsOptions {
  /**
   * WHICH INVARIANTS APPLY, and the answer is not "all of them" — because a
   * mid-run snapshot cannot be atomic.
   *
   * `TaskStore` is an async port with no "read everything at once" call, so
   * assembling a view means dozens of awaits, and other workers commit between
   * them. A mid-run view is therefore TORN: its tasks were read at one moment,
   * its attempt dump at a later one. Asserting a cross-read invariant against
   * torn evidence would report the driver's own read schedule as a store bug.
   *
   * `in-flight` runs exactly the invariants that survive tearing:
   * - anything internal to ONE synchronous dump (at most one running attempt
   *   per task, at most one live lease per task, attempt status/endedAt
   *   agreement) — the headline invariants, and the ones a broken claim path
   *   breaks first;
   * - anything internal to ONE read (`listEvents` gaplessness, a task's own
   *   terminal/`finishedAt` agreement, a half-written dead-letter mark);
   * - anything derived from the driver's own observations (fencing);
   * - the dependency gate (sound only for a view read newest-first — see
   *   {@link TaskInvariantView.tasks}) and lineage, which never changes.
   *
   * `quiescent` adds the rest — the cross-read pairings (`attemptCount` versus
   * the attempt rows, a held task's one running attempt, a terminal task's
   * released lease) and the ones that span two non-atomic WRITES the runner
   * itself makes in sequence (`markDeadLettered` then land). Those are exact
   * only when nothing is executing.
   */
  phase?: TaskInvariantPhase;
  /** Folded into every message, so a seeded failure names its seed. */
  label?: string;
}

/**
 * Grade a store dump. Returns the violations found, in a stable order; an empty
 * array means every invariant below held.
 *
 * Returning instead of throwing is deliberate: a randomized run wants ALL the
 * violations of one snapshot, not the first, and a caller that wants a throw
 * writes one line.
 */
export function checkTaskInvariants(
  view: TaskInvariantView,
  options: CheckTaskInvariantsOptions = {},
): string[] {
  const phase = options.phase ?? "quiescent";
  const violations: string[] = [];
  const say = (message: string): void => {
    violations.push(
      options.label === undefined ? message : `[${options.label}] ${message}`,
    );
  };

  const byId = new Map(view.tasks.map((task) => [task.taskId, task]));
  const attemptsByTask = groupBy(view.attempts, (a) => a.taskId);

  checkAttemptStructure(view, byId, attemptsByTask, phase, say);
  checkLeases(view, byId, attemptsByTask, phase, say);
  checkFencing(view, say);
  checkEventLogs(view, attemptsByTask, say);
  checkTaskFields(view, phase, say);
  checkDependencies(view, byId, say);
  checkLineage(view, byId, say);
  if (phase === "quiescent" && view.inFlightTaskIds.size > 0) {
    // Not a store invariant — a guard on the caller. A "quiescent" snapshot
    // taken while a worker is still executing would grade the exact torn
    // evidence the phase split exists to keep out.
    say(
      `snapshot claims quiescence but ${view.inFlightTaskIds.size} tasks are still in flight ` +
        `(${[...view.inFlightTaskIds].join(", ")})`,
    );
  }

  return violations;
}

/** The reads {@link snapshotTaskInvariants} needs — a narrow slice of `TaskStore`. */
export interface TaskInvariantReader {
  getTask(taskId: string): Promise<TaskRecord | null>;
  listEvents(taskId: string): Promise<TaskEventEnvelope[]>;
  listChildren(taskId: string): Promise<TaskRecord[]>;
}

export interface TaskInvariantSnapshotSource {
  reader: TaskInvariantReader;
  /** The tasks to grade, in CREATION order — the helper reverses them itself. */
  taskIds: readonly string[];
  observedLeases?: readonly ObservedLease[];
  dumpAttempts?: (() => readonly AttemptRecord[]) | undefined;
  dumpLiveLeases?: (() => readonly Lease[]) | undefined;
  inFlightTaskIds?: ReadonlySet<string>;
}

/**
 * Assemble a {@link TaskInvariantView} from public reads plus whatever dumps
 * the caller has.
 *
 * READS NEWEST FIRST, and takes the dumps LAST. Both orderings are what make a
 * mid-run view gradeable at all: dependents are read before their dependencies
 * (see {@link TaskInvariantView.tasks}), and the event logs are read before the
 * attempt dump, so every attempt an event names is already in it.
 */
export async function snapshotTaskInvariants(
  source: TaskInvariantSnapshotSource,
): Promise<TaskInvariantView> {
  const tasks: TaskRecord[] = [];
  const events = new Map<string, readonly TaskEventEnvelope[]>();
  const children = new Map<string, readonly string[]>();
  for (const taskId of [...source.taskIds].reverse()) {
    const task = await source.reader.getTask(taskId);
    if (!task) continue;
    tasks.push(task);
    events.set(taskId, await source.reader.listEvents(taskId));
    children.set(
      taskId,
      (await source.reader.listChildren(taskId)).map((child) => child.taskId),
    );
  }
  const attempts = source.dumpAttempts?.();
  const liveLeases = source.dumpLiveLeases?.();
  return {
    tasks,
    events,
    children,
    observedLeases: source.observedLeases ?? [],
    // Without a dump the completeness-dependent checks stand down rather than
    // running on partial evidence — see the module doc.
    attempts: attempts ?? [],
    attemptsAreComplete: attempts !== undefined,
    ...(liveLeases === undefined ? {} : { liveLeases }),
    inFlightTaskIds: source.inFlightTaskIds ?? new Set<string>(),
  };
}

type Say = (message: string) => void;

function groupBy<T>(
  items: readonly T[],
  key: (item: T) => string,
): Map<string, T[]> {
  const out = new Map<string, T[]>();
  for (const item of items) {
    const k = key(item);
    const bucket = out.get(k);
    if (bucket) bucket.push(item);
    else out.set(k, [item]);
  }
  return out;
}

/**
 * ONE RUNNING ATTEMPT PER TASK, and it belongs to a task that is actually held.
 *
 * This is the invariant the whole lease design exists to buy: two attempts of
 * one task marked running at the same time means two workers each believe they
 * own the work, which is the failure every other rule here is downstream of.
 */
function checkAttemptStructure(
  view: TaskInvariantView,
  byId: ReadonlyMap<string, TaskRecord>,
  attemptsByTask: ReadonlyMap<string, readonly AttemptRecord[]>,
  phase: "quiescent" | "in-flight",
  say: Say,
): void {
  for (const attempt of view.attempts) {
    if (!byId.has(attempt.taskId)) {
      say(
        `attempt ${attempt.attemptId} names task ${attempt.taskId}, which does not exist`,
      );
    }
  }

  for (const [taskId, attempts] of attemptsByTask) {
    const running = attempts.filter((a) => a.status === "running");
    if (running.length > 1) {
      say(
        `task ${taskId} has ${running.length} attempts marked running (${running
          .map((a) => a.attemptId)
          .join(", ")}); at most one may be`,
      );
    }
    for (const attempt of attempts) {
      if (attempt.status === "running" && attempt.endedAt !== undefined) {
        say(
          `attempt ${attempt.attemptId} of ${taskId} is running but carries endedAt ${attempt.endedAt}`,
        );
      }
      if (attempt.status !== "running" && attempt.endedAt === undefined) {
        say(
          `attempt ${attempt.attemptId} of ${taskId} ended as ${attempt.status} without an endedAt`,
        );
      }
    }

    if (!view.attemptsAreComplete) continue;
    // Attempt numbers come from `attemptCount` at create time, so a complete
    // dump must read back as exactly 1..n — a hole means a lost increment, a
    // repeat means two attempts were numbered off the same read. Internal to
    // the dump, so it holds mid-run too.
    const numbers = attempts.map((a) => a.attemptNumber).sort((a, b) => a - b);
    const expected = attempts.map((_, i) => i + 1);
    if (numbers.join(",") !== expected.join(",")) {
      say(
        `task ${taskId} has attempt numbers [${numbers.join(", ")}]; expected [${expected.join(", ")}]`,
      );
    }
    // `attemptCount` came from an EARLIER read than the dump — see the phase
    // doc on tearing.
    if (phase !== "quiescent") continue;
    const task = byId.get(taskId);
    if (task && task.attemptCount !== attempts.length) {
      say(
        `task ${taskId} reports attemptCount ${task.attemptCount} but ${attempts.length} attempt rows exist`,
      );
    }
  }

  if (!view.attemptsAreComplete || phase !== "quiescent") return;
  for (const task of view.tasks) {
    const attempts = attemptsByTask.get(task.taskId) ?? [];
    const running = attempts.filter((a) => a.status === "running").length;
    if (isHeld(task.status) && running !== 1) {
      say(
        `task ${task.taskId} is ${task.status} but has ${running} running attempts; a held task has exactly one`,
      );
    }
    if (isTerminal(task.status) && running !== 0) {
      say(
        `task ${task.taskId} is terminal (${task.status}) but still has ${running} running attempts`,
      );
    }
    if (task.status === "queued" && attempts.length !== 0) {
      say(
        `task ${task.taskId} is queued but has ${attempts.length} attempts; a queued task was never claimed`,
      );
    }
  }
}

/**
 * A LEASE IS A CLAIM, AND A CLAIM IS A RUNNING TASK.
 *
 * The bug this catches is the one `claim-next-concurrency.test.ts` was written
 * for, generalized: a lease that survives while the task row it names reverted
 * to `queued` is exactly the state that hands one task to a second worker.
 */
function checkLeases(
  view: TaskInvariantView,
  byId: ReadonlyMap<string, TaskRecord>,
  attemptsByTask: ReadonlyMap<string, readonly AttemptRecord[]>,
  phase: "quiescent" | "in-flight",
  say: Say,
): void {
  for (const lease of view.observedLeases) {
    const task = byId.get(lease.taskId);
    if (!task) {
      say(
        `observed lease ${lease.leaseToken} names unknown task ${lease.taskId}`,
      );
      continue;
    }
    // Everything below pairs a lease the driver observed against a task row
    // read at a different moment — exact only at quiescence.
    if (phase !== "quiescent") continue;
    // The claim that issued this lease transitioned the task `running` and
    // created an attempt in the same breath, and neither is reversible: a task
    // that was ever claimed carries the marks forever.
    if (task.startedAt === undefined) {
      say(
        `task ${lease.taskId} was claimed (lease ${lease.leaseToken}) but has no startedAt`,
      );
    }
    if (task.attemptCount < 1) {
      say(
        `task ${lease.taskId} was claimed (lease ${lease.leaseToken}) but reports attemptCount ${task.attemptCount}`,
      );
    }
    if (view.attemptsAreComplete) {
      const attempts = attemptsByTask.get(lease.taskId) ?? [];
      if (!attempts.some((a) => a.attemptId === lease.attemptId)) {
        say(
          `lease ${lease.leaseToken} names attempt ${lease.attemptId}, which is not an attempt of ${lease.taskId}`,
        );
      }
    }
  }

  const live = view.liveLeases;
  if (live === undefined) return;
  const perTask = groupBy(live, (l) => l.taskId);
  for (const [taskId, leases] of perTask) {
    if (leases.length > 1) {
      say(
        `task ${taskId} has ${leases.length} live leases; at most one may be live`,
      );
    }
    if (phase !== "quiescent") continue;
    const task = byId.get(taskId);
    if (!task) {
      say(`live lease on unknown task ${taskId}`);
      continue;
    }
    if (!isHeld(task.status)) {
      say(
        `task ${taskId} is ${task.status} but still holds a live lease (${leases.map((l) => l.leaseToken).join(", ")})`,
      );
    }
  }

  if (phase !== "quiescent") return;
  const leased = new Set(live.map((l) => l.taskId));
  for (const task of view.tasks) {
    // THE ORPHAN CHECK. A held task with no lease is unowned work that nothing
    // can finish and nothing can find: the task will never return to `queued`
    // (there is no such edge), no worker holds a token to write under, and
    // `expireStaleLeases` — the only recovery trigger there is — reports
    // LEASES, so a task with none is invisible to it forever.
    if (isHeld(task.status) && !leased.has(task.taskId)) {
      say(
        `task ${task.taskId} is ${task.status} but no live lease owns it; nothing can finish it and expireStaleLeases will never see it`,
      );
    }
  }
}

/**
 * FENCING TOKENS ONLY GO UP.
 *
 * Per task, because that is what fences a zombie out: attempt 2's token must
 * beat attempt 1's or the paused worker that wakes up cannot be told apart from
 * the live one. Globally unique, because the counter is store-global by
 * contract — two leases sharing a token means two `acquireLease` calls read the
 * counter before either wrote it, which is the multi-writer bug the token
 * exists to make detectable.
 */
function checkFencing(view: TaskInvariantView, say: Say): void {
  const ordered = [...view.observedLeases].sort(
    (a, b) => a.observedAt - b.observedAt,
  );
  const lastPerTask = new Map<string, ObservedLease>();
  for (const lease of ordered) {
    const previous = lastPerTask.get(lease.taskId);
    if (previous && lease.fencingToken <= previous.fencingToken) {
      say(
        `task ${lease.taskId}: fencing token went ${previous.fencingToken} → ${lease.fencingToken} ` +
          `(leases ${previous.leaseToken} → ${lease.leaseToken}); it must strictly increase`,
      );
    }
    lastPerTask.set(lease.taskId, lease);
  }

  const seen = new Map<number, ObservedLease>();
  for (const lease of ordered) {
    const clash = seen.get(lease.fencingToken);
    if (clash) {
      say(
        `fencing token ${lease.fencingToken} was issued twice ` +
          `(${clash.leaseToken} on ${clash.taskId}, ${lease.leaseToken} on ${lease.taskId})`,
      );
    }
    seen.set(lease.fencingToken, lease);
  }
}

/**
 * THE EVENT LOG IS GAPLESS FROM 0, AND EVERY EVENT BELONGS TO A REAL ATTEMPT.
 *
 * The store itself only rejects `seq <= last` — a gap is legal to it, because
 * numbering belongs to the emitter (`nextSeq` + `createTaskEventWriter`). What
 * makes gaplessness an invariant is the pair: an emitter that always starts at
 * `nextSeq` and a store that never re-stamps. A retry crossing attempts must
 * therefore CONTINUE the sequence, not restart it, and a hole here is exactly
 * the dropped-event symptom `seq` exists to expose.
 */
function checkEventLogs(
  view: TaskInvariantView,
  attemptsByTask: ReadonlyMap<string, readonly AttemptRecord[]>,
  say: Say,
): void {
  for (const [taskId, events] of view.events) {
    const seqs = events.map((e) => e.seq);
    for (let i = 0; i < seqs.length; i += 1) {
      if (seqs[i] !== i) {
        say(
          `task ${taskId} event log is not gapless from 0: seq[${i}] is ${seqs[i]} (full: [${seqs.join(", ")}])`,
        );
        break;
      }
    }
    const ids = new Set<string>();
    for (const event of events) {
      if (ids.has(event.eventId)) {
        say(`task ${taskId} has duplicate eventId ${event.eventId}`);
      }
      ids.add(event.eventId);
    }
    if (!view.attemptsAreComplete) continue;
    const attemptIds = new Set(
      (attemptsByTask.get(taskId) ?? []).map((a) => a.attemptId),
    );
    for (const event of events) {
      // An event stamped with an attempt that is not this task's is a zombie
      // writer that got past the lease check.
      if (event.attemptId !== undefined && !attemptIds.has(event.attemptId)) {
        say(
          `task ${taskId} event ${event.eventId} (seq ${event.seq}) names attempt ${event.attemptId}, which is not an attempt of this task`,
        );
      }
    }
  }
}

/**
 * A FINISHED TASK LOOKS FINISHED, AND A POISONED ONE WAS ACTUALLY BURIED.
 *
 * `markDeadLettered` writes the poison marks and nothing else — the row alone
 * stops no worker, so the caller must land the task too (that is what
 * `SingleProcessTaskRunner.deadLetter` does). A dead-lettered row that is still
 * runnable is a task the queue gave up on and would hand out again. The poison
 * COUNT is not one of those marks: `TaskStore.endAttempt` increments it itself
 * when an attempt ends `abandoned`, so nothing here has to be told about it.
 */
function checkTaskFields(
  view: TaskInvariantView,
  phase: TaskInvariantPhase,
  say: Say,
): void {
  for (const task of view.tasks) {
    if (isTerminal(task.status) && task.finishedAt === undefined) {
      say(
        `task ${task.taskId} is terminal (${task.status}) but has no finishedAt`,
      );
    }
    if (!isTerminal(task.status) && task.finishedAt !== undefined) {
      say(
        `task ${task.taskId} is ${task.status} but already carries finishedAt ${task.finishedAt}`,
      );
    }
    // `startedAt` and `attemptCount` are written by ONE claim, but not
    // necessarily by one atomic write (the memory adapter transitions the task
    // and then creates the attempt), so the pairing is exact only at rest.
    if (
      phase === "quiescent" &&
      task.attemptCount > 0 &&
      task.startedAt === undefined
    ) {
      say(
        `task ${task.taskId} has ${task.attemptCount} attempts but no startedAt`,
      );
    }
    if (
      phase === "quiescent" &&
      task.attemptCount === 0 &&
      task.startedAt !== undefined
    ) {
      say(
        `task ${task.taskId} was never attempted but carries startedAt ${task.startedAt}`,
      );
    }
    const poisoned = task.deadLetteredAt !== undefined;
    if (poisoned !== (task.deadLetterReason !== undefined)) {
      say(
        `task ${task.taskId} has a half-written dead-letter mark (at=${String(task.deadLetteredAt)}, reason=${String(task.deadLetterReason)})`,
      );
    }
    // `markDeadLettered` and the land that follows it are two writes even in
    // the production runner, so a mid-run snapshot may legitimately land
    // between them.
    if (phase === "quiescent" && poisoned && !isTerminal(task.status)) {
      say(
        `task ${task.taskId} is dead-lettered but still ${task.status}; the poison row alone stops nothing`,
      );
    }
    if (poisoned && task.attemptCount < 1) {
      say(
        `task ${task.taskId} is dead-lettered with attemptCount ${task.attemptCount}; poison is earned by attempts`,
      );
    }
  }
}

/**
 * NOTHING RUNS AHEAD OF ITS DEPENDENCIES.
 *
 * `claimNext` is the only gate, so this is the assertion that grades it. It is
 * one-directional on purpose: a task may be `failed` or `cancelled` with an
 * unfinished dependency — that is the settle path doing its job — but it may
 * never have RUN with one.
 */
function checkDependencies(
  view: TaskInvariantView,
  byId: ReadonlyMap<string, TaskRecord>,
  say: Say,
): void {
  for (const task of view.tasks) {
    if (!isHeld(task.status) && task.status !== "completed") continue;
    for (const dependencyId of task.dependsOn ?? []) {
      const dependency = byId.get(dependencyId);
      if (!dependency) {
        say(
          `task ${task.taskId} is ${task.status} but its dependency ${dependencyId} does not exist`,
        );
        continue;
      }
      if (dependency.status !== "completed") {
        say(
          `task ${task.taskId} is ${task.status} but its dependency ${dependencyId} is ${dependency.status}`,
        );
      }
      if (dependency.deadLetteredAt !== undefined) {
        say(
          `task ${task.taskId} is ${task.status} but its dependency ${dependencyId} was dead-lettered`,
        );
      }
    }
  }
}

/** `listChildren` and `parentTaskId` are two views of one edge; they must agree. */
function checkLineage(
  view: TaskInvariantView,
  byId: ReadonlyMap<string, TaskRecord>,
  say: Say,
): void {
  for (const [parentId, childIds] of view.children) {
    for (const childId of childIds) {
      const child = byId.get(childId);
      if (!child) {
        say(`listChildren(${parentId}) returned unknown task ${childId}`);
        continue;
      }
      if (child.parentTaskId !== parentId) {
        say(
          `listChildren(${parentId}) returned ${childId}, whose parentTaskId is ${String(child.parentTaskId)}`,
        );
      }
    }
  }
  for (const task of view.tasks) {
    const parentId = task.parentTaskId;
    if (parentId === undefined) continue;
    const listed = view.children.get(parentId);
    // Only graded when the driver actually asked for that parent's children.
    if (listed !== undefined && !listed.includes(task.taskId)) {
      say(
        `task ${task.taskId} names parent ${parentId}, but listChildren(${parentId}) does not include it`,
      );
    }
  }
}
