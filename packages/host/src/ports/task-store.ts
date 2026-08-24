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
  queued: Object.freeze(["running", "cancelled"] as const),
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
   */
  payload: Record<string, unknown>;
  error?: string;
  attemptCount: number;
  /** Attempts that died without a clean end — the dead-letter trigger. */
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
  availableAt?: string;
}

/** Fields a transition may write alongside the status change, atomically. */
export interface TaskPatch {
  startedAt?: string;
  finishedAt?: string;
  error?: string;
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

export interface AppendEventsOptions {
  /** The writer's proof of ownership. Stale token ⇒ `LeaseLostError`. */
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
   */
  createTask(input: CreateTaskInput): Promise<TaskRecord>;
  getTask(taskId: string): Promise<TaskRecord | null>;

  /**
   * Compare-and-set the task's status.
   *
   * MUST reject with {@link InvalidTaskTransitionError} when the current status
   * is not in `from` (someone else moved it first — a lost race, not a retryable
   * hiccup) or when `from → to` is not in {@link TASK_TRANSITIONS}. Passing the
   * expected `from` set rather than blind-writing is what makes cancel-vs-finish
   * races resolve to exactly one winner.
   */
  transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
  ): Promise<TaskRecord>;

  createAttempt(input: CreateAttemptInput): Promise<AttemptRecord>;
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
   * The `seq` the next appended event must carry (0 for an empty task). A retry
   * passes it to `runChat` as `firstSeq` so several provider passes produce ONE
   * unbroken sequence under one task id.
   */
  nextSeq(taskId: string): Promise<number>;

  /**
   * Atomically claim the highest-priority claimable task whose scope is free (and
   * whose kind passes `kinds`, when given), creating its attempt and lease.
   * Returns null when there is nothing to do.
   */
  claimNext(input: ClaimNextInput): Promise<ClaimedTask | null>;

  markDeadLettered(taskId: string, reason: string): Promise<TaskRecord>;
}
