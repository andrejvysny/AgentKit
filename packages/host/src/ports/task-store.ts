import type { TaskEventEnvelope } from "@agentkit/contracts";
import { InvalidTaskTransitionError } from "../errors.js";

/**
 * Lifecycle of a durable task.
 *
 * `queued` is not a formality: a submitted task is persisted and acknowledged
 * before any worker touches it, so a crash between "the host accepted the work"
 * and "the executor started" loses nothing. `waiting_approval` exists because a
 * task that staged a write and is waiting on a human is neither running nor
 * finished, and calling it either would make the queue's recovery pass do the
 * wrong thing.
 *
 * Nothing in this repository currently produces `waiting_approval`: a staged
 * write returns `pending` to the model and the chat-turn executor completes the
 * task. It is reserved for a host that parks a task on the decision and resumes
 * it after — the state and its transitions exist so that host does not fork this
 * table.
 */
export type TaskStatus =
  | "queued"
  | "running"
  | "waiting_approval"
  | "completed"
  | "failed"
  | "cancelled";

/**
 * Lifecycle of ONE attempt at a task. `abandoned` is the crash outcome: the
 * lease expired with the attempt still marked running, so recovery ends it that
 * way rather than claiming knowledge of success or failure it does not have.
 */
export type AttemptStatus =
  | "running"
  | "completed"
  | "failed"
  | "abandoned"
  | "cancelled";

/**
 * The ONLY legal task status changes. Frozen and exported so every store
 * implementation validates against the same table instead of re-deriving the
 * rules — a divergence between two adapters would be a silent data bug.
 *
 * Same-state entries are absent on purpose: `running → running` would let two
 * workers both believe they started the task.
 */
export const TASK_TRANSITIONS: Readonly<
  Record<TaskStatus, readonly TaskStatus[]>
> = Object.freeze({
  // `queued → failed` exists for ONE caller: the dependency cascade in
  // `claimNext`. A task whose dependency failed or was dead-lettered can never
  // become runnable, so the claim settles it where it stands instead of
  // claiming it, and a task that never started still has to be able to end
  // `failed` — see {@link evaluateTaskDependencies}.
  queued: Object.freeze(["running", "cancelled", "failed"] as const),
  running: Object.freeze([
    "waiting_approval",
    "completed",
    "failed",
    "cancelled",
  ] as const),
  waiting_approval: Object.freeze([
    "running",
    "completed",
    "failed",
    "cancelled",
  ] as const),
  completed: Object.freeze([] as const),
  failed: Object.freeze([] as const),
  cancelled: Object.freeze([] as const),
});

/** True when `from → to` is a legal task transition. */
export function isTaskTransitionAllowed(
  from: TaskStatus,
  to: TaskStatus,
): boolean {
  return TASK_TRANSITIONS[from].includes(to);
}

/**
 * Throw unless `from → to` is legal. The one piece of logic the port catalog
 * carries: a shared assertion is what keeps every adapter honest.
 */
export function assertTaskTransition(from: TaskStatus, to: TaskStatus): void {
  if (!isTaskTransitionAllowed(from, to)) {
    throw new InvalidTaskTransitionError(
      `Illegal task transition ${from} → ${to}.`,
      { from, to, allowed: TASK_TRANSITIONS[from] },
    );
  }
}

/**
 * One dependency as the queue reads it while gating a claim — the two facts
 * that decide the gate, and nothing else, so a store can answer from a narrow
 * projection instead of loading whole records.
 */
export interface TaskDependencyState {
  taskId: string;
  /** null when the row is gone. */
  status: TaskStatus | null;
  /** `deadLetteredAt` is set — the queue gave up on it, whatever its status says. */
  deadLettered: boolean;
}

/**
 * What a queued task's dependencies say about it right now: claim it, skip it,
 * or end it here.
 */
export type TaskDependencyVerdict =
  | { kind: "ready" }
  /** Some dependency is still in flight; try again on a later claim. */
  | { kind: "blocked" }
  /**
   * No dependency can ever complete now. The claim transitions the dependent to
   * `to` INSTEAD of claiming it — `error` is set for `failed` and absent for
   * `cancelled`, because a cancellation is not a failure and recording one as
   * an error would make every "why did this fail?" dashboard lie.
   */
  | { kind: "settle"; to: "failed" | "cancelled"; error?: string };

/**
 * The dependency gate, shared so every adapter reaches the same verdict from
 * the same facts — the second piece of logic this port catalog carries, for the
 * same reason as {@link assertTaskTransition}: two stores disagreeing about
 * when a task becomes claimable is a silent data bug, not a style difference.
 *
 * A BAD dependency beats a pending one, whatever the order: a task whose
 * dependency already failed is doomed, and parking it behind a sibling that is
 * still running only delays the news. Among several bad ones the FIRST in
 * `dependsOn` order decides, so the verdict is a function of the record rather
 * than of scan order.
 *
 * A dependency the store cannot find counts as failed. `createTask` rejects
 * unknown ids ({@link UnknownDependencyError}), so this is unreachable by
 * construction — but if a row ever does vanish, failing the dependent beats
 * blocking it forever on something that can never complete.
 */
export function evaluateTaskDependencies(
  dependencies: readonly TaskDependencyState[],
): TaskDependencyVerdict {
  let blocked = false;
  for (const dependency of dependencies) {
    if (
      dependency.status === null ||
      dependency.status === "failed" ||
      dependency.deadLettered
    ) {
      return {
        kind: "settle",
        to: "failed",
        error: `dependency_failed: ${dependency.taskId}`,
      };
    }
    if (dependency.status === "cancelled") {
      return { kind: "settle", to: "cancelled" };
    }
    if (dependency.status !== "completed") blocked = true;
  }
  return blocked ? { kind: "blocked" } : { kind: "ready" };
}

export interface TaskRecord {
  taskId: string;
  /**
   * What kind of work this is, and therefore which executor runs it. Host-chosen
   * (see `CHAT_TURN_TASK_KIND` and the reserved prefixes documented beside it);
   * the store treats it as an opaque string it filters and returns.
   */
  kind: string;
  /**
   * Serialization key. Tasks sharing a scope never execute concurrently, so two
   * turns cannot write the same document at once. Usually the chat id, but a
   * host that writes a shared document scopes on the document instead.
   */
  scopeId: string;
  status: TaskStatus;
  /** Higher tasks first; combined with age when the queue claims work. */
  priority: number;
  enqueuedAt: string;
  /** Not claimable before this instant (backoff / scheduled work). */
  availableAt: string;
  startedAt?: string;
  finishedAt?: string;
  /**
   * What to run, in whatever shape the executor for this `kind` understands —
   * for a chat turn: the chat, model, provider, and the message ids the turn
   * owns.
   *
   * MUST be serializable: a durable store round-trips it through its own
   * encoding, and the reference adapters copy it on the way in and out so a
   * caller that keeps mutating the object it submitted cannot edit a task
   * already queued.
   */
  payload: Record<string, unknown>;
  /**
   * The task that spawned this one, when an executor fanned work out (see
   * `TaskExecutionContext.spawnChild`).
   *
   * LINEAGE ONLY — it is not a dependency and not a lifecycle coupling. A child
   * runs the moment the queue can claim it, whether or not the parent is still
   * running, because the parent usually spawns children precisely so they can
   * proceed without it. What lineage buys is the two questions a fan-out host
   * actually asks: "what did this task set off?"
   * ({@link TaskStore.listChildren}) and "cancel this whole branch"
   * (`TaskService.cancelTask`). A parent that must WAIT for its children
   * expresses that with a third task that `dependsOn` them — the continuation
   * pattern — not by conflating the two edges.
   */
  parentTaskId?: string;
  /**
   * Task ids that must reach `completed` before this task may be claimed.
   *
   * IMMUTABLE AFTER CREATE, and every id must already exist when the dependent
   * is written ({@link UnknownDependencyError} otherwise). Those two rules
   * together are what make the graph acyclic by construction: an edge can only
   * point at an older row, so there is no order of writes that produces a
   * cycle, and nothing can add one later. A queue that can deadlock on
   * committed data is a queue that needs a human at 3am; this one cannot.
   *
   * The gate is enforced in {@link TaskStore.claimNext} — dependency state is
   * queue semantics, not orchestration — and a dependency that ends badly
   * settles the dependent rather than parking it forever. See
   * {@link evaluateTaskDependencies}.
   */
  dependsOn?: string[];
  /**
   * Last-known progress of a running task: an OVERWRITTEN snapshot ("42 of 900
   * files"), never an append.
   *
   * Deliberately not an event. The event log is the durable, ordered,
   * replayable record of what happened and it is what a UI replays after a
   * crash — writing a heartbeat percentage into it every second would bloat
   * that log with entries nobody will ever replay, and force every consumer to
   * fold thousands of superseded numbers to learn one current value. Progress
   * is the opposite kind of data: only the latest matters, and losing an
   * intermediate value costs nothing. See {@link TaskStore.updateProgress}.
   */
  progress?: Record<string, unknown>;
  error?: string;
  attemptCount: number;
  /**
   * Attempts that died without a clean end (`abandoned`), as counted by
   * whoever gave up on the task.
   *
   * NOT the dead-letter trigger — `attemptCount` is (the runner compares it
   * with its own `maxAttempts`). This is the diagnosis that travels with the
   * dead letter: "three attempts, and all three were abandoned" is a crashing
   * worker, while "three attempts, none abandoned" is work that fails cleanly.
   * Written through {@link TaskPatch.poisonCount} on the transition that lands
   * the task, because {@link TASK_TRANSITIONS} has no `running → running` edge
   * to carry a mid-flight increment.
   */
  poisonCount: number;
  deadLetteredAt?: string;
  deadLetterReason?: string;
}

export interface CreateTaskInput {
  taskId: string;
  kind: string;
  scopeId: string;
  payload: Record<string, unknown>;
  priority?: number;
  /**
   * Not claimable before this instant. NORMALIZED by the store to a UTC ISO
   * string (`new Date(x).toISOString()`) — see {@link TaskPatch.availableAt}.
   */
  availableAt?: string;
  /** Lineage — see {@link TaskRecord.parentTaskId}. Must already exist. */
  parentTaskId?: string;
  /** Gate — see {@link TaskRecord.dependsOn}. Every id must already exist. */
  dependsOn?: string[];
}

/** Fields a transition may write alongside the status change, atomically. */
export interface TaskPatch {
  startedAt?: string;
  finishedAt?: string;
  error?: string;
  /**
   * Reschedule the task's earliest claim.
   *
   * NORMALIZED to a UTC ISO instant before it is stored, and an unparsable
   * value is REJECTED rather than written. Both matter because a store compares
   * this field as TEXT: `2026-01-01T01:30:00-05:00` names 06:30Z but sorts
   * before `2026-01-01T02:00:00.000Z`, so an un-normalized offset-form backoff
   * is claimed hours before it is due — and one adapter parsing it as a date
   * while another compares the raw string is a divergence no conformance case
   * would see unless it used an offset.
   */
  availableAt?: string;
  priority?: number;
  poisonCount?: number;
  payload?: Record<string, unknown>;
}

export interface AttemptRecord {
  attemptId: string;
  taskId: string;
  /** 1-based; `attemptCount` at the time the attempt was created. */
  attemptNumber: number;
  status: AttemptStatus;
  ownerId: string;
  startedAt: string;
  endedAt?: string;
  error?: string;
}

export interface CreateAttemptInput {
  attemptId: string;
  taskId: string;
  ownerId: string;
}

export interface EndAttemptInput {
  attemptId: string;
  status: Exclude<AttemptStatus, "running">;
  error?: string;
  /**
   * Proof of ownership — see {@link FencedWriteOptions}. Absent means the
   * write is not fenced, which is what recovery needs: it ends the attempt of
   * a lease that has already been expired and deleted, so there is no token
   * left to prove anything with.
   */
  leaseToken?: string;
}

/**
 * Exclusive, expiring ownership of a task.
 *
 * `leaseToken` proves "I am the current owner" on every write. `fencingToken` is
 * monotonic across ALL leases, so a resumed-from-the-dead worker holding an old
 * lease can be rejected by comparison even if it somehow re-acquires: the
 * classic fix for the process that paused for a minute and woke up believing it
 * still owned the task.
 */
export interface Lease {
  taskId: string;
  attemptId: string;
  ownerId: string;
  leaseToken: string;
  fencingToken: number;
  expiresAt: string;
}

export interface AcquireLeaseInput {
  taskId: string;
  attemptId: string;
  ownerId: string;
  ttlMs: number;
}

/**
 * Ownership proof for the writes that END a task or an attempt.
 *
 * `appendEvents` and `updateProgress` have always demanded a `leaseToken`,
 * because two attempts interleaving into one event stream is obviously wrong.
 * The TERMINAL writes did not, and that was the bigger hole: an attempt whose
 * lease expired mid-tool-call (recovery has since started attempt 2) would
 * still transition the task `completed` and end its own attempt, burying the
 * live attempt's verdict under a zombie's. A runner cannot close that from
 * outside — its `renewLease` pre-check and the write it guards are separated by
 * awaits — so the proof has to be checked INSIDE the adapter's own
 * transaction, next to the write.
 *
 * OPTIONAL, and deliberately so. A host that lands a task from outside any
 * lease (a cancel from an HTTP handler, a boot-time reaper, `TaskService`) has
 * no token to offer, and refusing those writes would break the very recovery
 * paths that repair a crashed run. Absent means "unfenced, as before"; present
 * means "verify or refuse".
 */
export interface FencedWriteOptions {
  /**
   * The writer's current lease token. When given, the store MUST verify inside
   * the same transaction as the write that this token names the task's CURRENT
   * lease, and throw {@link LeaseLostError} otherwise.
   */
  leaseToken?: string;
}

export interface AppendEventsOptions {
  /** The writer's proof of ownership. Stale token ⇒ `LeaseLostError`. */
  leaseToken: string;
}

export interface UpdateProgressOptions {
  /**
   * The writer's proof of ownership — the SAME check `appendEvents` makes, for
   * the same reason: a zombie worker whose lease was taken over must not
   * overwrite the live attempt's progress with a snapshot from the run nobody
   * is watching any more. Stale token ⇒ `LeaseLostError`.
   */
  leaseToken: string;
}

export interface ListEventsOptions {
  afterSeq?: number;
  limit?: number;
}

export interface ClaimNextInput {
  ownerId: string;
  now: Date;
  /** Scopes with a task already executing; the claim must skip them. */
  scopesBusy: string[];
  /**
   * When present, only tasks whose `kind` is in this list may be claimed.
   *
   * A deployment can run several worker processes that register DIFFERENT
   * executor sets (the GPU box takes `index.embed`, the web box takes
   * `chat.turn`). Without the filter a worker claims work it cannot execute,
   * and the task fails on `ExecutorNotFoundError` instead of waiting for the
   * process that could have run it. Absent means "any kind".
   */
  kinds?: string[];
}

export interface ClaimedTask {
  task: TaskRecord;
  attempt: AttemptRecord;
  lease: Lease;
}

/**
 * Durable task state: lifecycle, attempts, leases, and the event log.
 *
 * The event log is the product of a task — everything a UI replays and
 * everything a crash recovery reads — so its two invariants are contractual, not
 * advisory (see {@link TaskStore.appendEvents}).
 */
export interface TaskStore {
  /**
   * Persist a new `queued` task.
   *
   * MUST reject a `taskId` that already exists with {@link DuplicateTaskError},
   * never silently overwrite: the id is the caller's idempotency key (a retried
   * HTTP submit, a replayed message), and quietly replacing the row would
   * discard an in-flight task's payload, status, and attempt history while
   * leaving its events behind.
   *
   * MUST also reject with {@link UnknownDependencyError} when `parentTaskId` or
   * any `dependsOn` id is not already in the store, and when `dependsOn`
   * contains the task's own id. That check is the acyclicity proof — see
   * {@link TaskRecord.dependsOn} — so a store that skipped it would accept a
   * graph its own `claimNext` can never drain.
   */
  createTask(input: CreateTaskInput): Promise<TaskRecord>;
  getTask(taskId: string): Promise<TaskRecord | null>;

  /**
   * Tasks whose `parentTaskId` is `taskId` — one level, not the whole subtree.
   *
   * One level because the caller that needs the subtree
   * (`TaskService.cancelTask`) is already walking breadth-first and can ask
   * again; a store-side recursive walk would make every adapter implement a
   * traversal to serve one consumer, and would return a snapshot that is stale
   * by the time it is read anyway.
   */
  listChildren(taskId: string): Promise<TaskRecord[]>;

  /**
   * Every task in a scope, whatever its status.
   *
   * Deliberately the NARROWEST query that answers the one question a caller
   * outside this port actually has — "is anything still live in this chat?",
   * which `ConversationService.deleteChat` asks before it deletes anything. A
   * general `listTasks(filter)` would have to grow statuses, kinds, ordering
   * and paging, and every adapter would owe an implementation of all of it to
   * serve one consumer that wants a handful of rows from one indexed lookup.
   *
   * No paging, for the same reason: a scope is a serialization key (usually one
   * chat), so its task count is bounded by how much work that one conversation
   * has ever queued, and a caller that must inspect all of them cannot use a
   * page anyway.
   */
  listByScope(scopeId: string): Promise<TaskRecord[]>;

  /**
   * Delete every task in a scope — with its attempts, its lease and its event
   * log — and return how many TASKS were removed.
   *
   * REFUSES with {@link ChatBusyError} (`chat_busy`) when ANY task in the scope
   * is `running` or `waiting_approval`, and then deletes nothing at all. THE
   * STORE OWNS THIS GUARANTEE, and an implementation MUST make the check and
   * the deletes ONE synchronous statement or transaction with no `await`
   * between them.
   *
   * WHY THE STORE AND NOT THE CALLER. `ConversationService.deleteChat` still
   * checks first — it wants to refuse before it has deleted the conversation —
   * but its check cannot be the guarantee. It reads the scope and then deletes
   * inside one async transaction, with an `await` in between, and a
   * check-then-act invariant that spans an `await` is not atomic no matter how
   * well-wrapped the transaction is. The reference adapters now QUEUE an
   * unrelated caller's write behind an open transaction rather than letting it
   * flatten in (that is how a concurrent `claimNext` used to move a task
   * `queued → running` inside the gap), but that is an adapter's courtesy, not
   * something this port can promise of every store. The hazard is a CLASS: an
   * invariant of that shape has to be enforced by a single synchronous
   * statement or transaction inside the adapter — which is what this method
   * does.
   *
   * `queued` is deliberately NOT live: nothing has been spent on it, and
   * refusing on one would make a chat undeletable for as long as anything sat
   * merely enqueued behind it. Force-cancelling what IS live is a different
   * operation with different consequences (it ends a provider call mid-flight),
   * so this method refuses instead of inventing it; the caller cancels
   * explicitly and deletes after.
   *
   * The event log goes with the tasks: it is the record of what those attempts
   * did, and orphaning it would leave rows nothing can ever name again.
   */
  deleteByScope(scopeId: string): Promise<number>;

  /**
   * Compare-and-set the task's status.
   *
   * MUST reject with {@link InvalidTaskTransitionError} when the current status
   * is not in `from` (someone else moved it first — a lost race, not a retryable
   * hiccup) or when `from → to` is not in {@link TASK_TRANSITIONS}. Passing the
   * expected `from` set rather than blind-writing is what makes cancel-vs-finish
   * races resolve to exactly one winner.
   *
   * With `opts.leaseToken`, the CAS is fenced as well: the store MUST verify
   * ownership inside the same transaction and reject a stale token with
   * {@link LeaseLostError} — see {@link FencedWriteOptions}.
   */
  transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord>;

  createAttempt(input: CreateAttemptInput): Promise<AttemptRecord>;
  /**
   * Close an attempt with its outcome.
   *
   * With {@link EndAttemptInput.leaseToken}, fenced the same way
   * {@link transitionTask} is: the token must name the CURRENT lease of the
   * attempt's task, or the write is refused with {@link LeaseLostError}.
   */
  endAttempt(input: EndAttemptInput): Promise<AttemptRecord>;

  acquireLease(input: AcquireLeaseInput): Promise<Lease>;
  /** MUST throw {@link LeaseLostError} when the token is no longer current. */
  renewLease(leaseToken: string, ttlMs: number): Promise<Lease>;
  releaseLease(leaseToken: string): Promise<void>;
  /** Ends leases past their expiry and returns them, so recovery can act. */
  expireStaleLeases(now: Date): Promise<Lease[]>;

  /**
   * Append events to the task's log.
   *
   * Contract — an implementation MUST:
   * - reject a stale `leaseToken` with {@link LeaseLostError}, so a zombie
   *   worker cannot interleave its events with the live attempt's;
   * - reject non-monotonic `seq` with {@link SeqConflictError} — a repeat or a
   *   gap breaks the consumer's only means of detecting a dropped event.
   *
   * It MUST NOT re-stamp `seq`: the emitter owns numbering (core's
   * `RunChatInput.firstSeq` inside a chat pass, `createTaskEventWriter`
   * elsewhere), so a store that renumbered would desync the ids already streamed
   * to a client.
   *
   * A REPEATED `eventId` is rejected with {@link SeqConflictError} too. The
   * guaranteed scope of that uniqueness is ONE TASK — that is all a consumer
   * deduping a replayed stream needs, and all a store can promise cheaply. A
   * store MAY enforce it more widely (the sqlite adapter's `event_id` index is
   * UNIQUE across the whole table, so a collision between two tasks fails there
   * and passes on the memory adapter); ids come from `IdGenerator.eventId`, so
   * a cross-task collision means a broken generator either way.
   *
   * The element type is the task-kind-agnostic {@link TaskEventEnvelope}: the
   * store orders by `seq` and dedups by `eventId` and reads nothing else, so one
   * log holds a chat turn's `AiRunEvent`s and another kind's vocabulary without
   * knowing either.
   */
  appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void>;

  listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]>;

  /**
   * Overwrite the task's {@link TaskRecord.progress} snapshot.
   *
   * REPLACES rather than merges: the writer owns the whole shape, and a merge
   * would make a field that disappeared from one snapshot linger from the last.
   * MUST reject a stale `leaseToken` with {@link LeaseLostError}.
   *
   * NEVER touches the event log, and emits nothing. Progress is state, not
   * history — see {@link TaskRecord.progress}.
   */
  updateProgress(
    taskId: string,
    progress: Record<string, unknown>,
    opts: UpdateProgressOptions,
  ): Promise<TaskRecord>;

  /**
   * The `seq` the next appended event must carry (0 for an empty task). A retry
   * passes it to `runChat` as `firstSeq` so several provider passes produce ONE
   * unbroken sequence under one task id.
   */
  nextSeq(taskId: string): Promise<number>;

  /**
   * Atomically claim the highest-priority claimable task whose scope is free (and
   * whose kind passes `kinds`, when given), creating its attempt and lease.
   * Returns null when there is nothing to do.
   *
   * DEPENDENCIES ARE PART OF CLAIMABILITY, and this is where they are enforced:
   * a task with an unfinished `dependsOn` entry is skipped, and the claim moves
   * on to the next candidate in the same call.
   *
   * A candidate whose dependency ended BADLY is settled here too — transitioned
   * `failed` (with `dependency_failed: <depTaskId>`) or `cancelled` per
   * {@link evaluateTaskDependencies} — instead of being claimed, and the scan
   * continues past it. Doing that lazily, on the claim path, is deliberate:
   * nothing is ever re-enqueued, no background reaper has to exist, and the
   * store stays the single truth about what is runnable. A chain settles over
   * successive claims, each sweep resolving what the previous one unblocked.
   */
  claimNext(input: ClaimNextInput): Promise<ClaimedTask | null>;

  /**
   * Stamp the task as poisoned — the row that says "stop feeding this work".
   *
   * Fenced by `opts.leaseToken` on the same terms as {@link transitionTask}:
   * present, the token must name the task's current lease.
   */
  markDeadLettered(
    taskId: string,
    reason: string,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord>;
}
