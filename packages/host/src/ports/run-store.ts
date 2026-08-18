import type { AiRunEvent } from "@agentkit/contracts";
import { InvalidRunTransitionError } from "../errors.js";

/**
 * Lifecycle of a durable run.
 *
 * `queued` is not a formality: a submitted turn is persisted and acknowledged
 * before any worker touches it, so a crash between "user hit send" and "model
 * started" loses nothing. `waiting_approval` exists because a run that staged a
 * write and is waiting on a human is neither running nor finished, and calling
 * it either would make the queue's recovery pass do the wrong thing.
 *
 * Nothing in this repository currently produces `waiting_approval`: a staged
 * write returns `pending` to the model and `TurnRunner` completes the run. It is
 * reserved for a host that parks a run on the decision and resumes it after —
 * the state and its transitions exist so that host does not fork this table.
 */
export type RunStatus =
  | "queued"
  | "running"
  | "waiting_approval"
  | "completed"
  | "failed"
  | "cancelled";

/**
 * Lifecycle of ONE attempt at a run. `abandoned` is the crash outcome: the lease
 * expired with the attempt still marked running, so recovery ends it that way
 * rather than claiming knowledge of success or failure it does not have.
 */
export type AttemptStatus =
  | "running"
  | "completed"
  | "failed"
  | "abandoned"
  | "cancelled";

/**
 * The ONLY legal run status changes. Frozen and exported so every store
 * implementation validates against the same table instead of re-deriving the
 * rules — a divergence between two adapters would be a silent data bug.
 *
 * Same-state entries are absent on purpose: `running → running` would let two
 * workers both believe they started the run.
 */
export const RUN_TRANSITIONS: Readonly<Record<RunStatus, readonly RunStatus[]>> =
  Object.freeze({
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

/** True when `from → to` is a legal run transition. */
export function isRunTransitionAllowed(
  from: RunStatus,
  to: RunStatus,
): boolean {
  return RUN_TRANSITIONS[from].includes(to);
}

/**
 * Throw unless `from → to` is legal. The one piece of logic the port catalog
 * carries: a shared assertion is what keeps every adapter honest.
 */
export function assertRunTransition(from: RunStatus, to: RunStatus): void {
  if (!isRunTransitionAllowed(from, to)) {
    throw new InvalidRunTransitionError(
      `Illegal run transition ${from} → ${to}.`,
      { from, to, allowed: RUN_TRANSITIONS[from] },
    );
  }
}

export interface RunRecord {
  runId: string;
  chatId: string;
  /**
   * Serialization key. Runs sharing a scope never execute concurrently, so two
   * turns cannot write the same document at once. Usually the chat id, but a
   * host that writes a shared document scopes on the document instead.
   */
  scopeId: string;
  status: RunStatus;
  /** Higher runs first; combined with age when the queue claims work. */
  priority: number;
  enqueuedAt: string;
  /** Not claimable before this instant (backoff / scheduled work). */
  availableAt: string;
  startedAt?: string;
  finishedAt?: string;
  /** What to run: model, provider, the message ids this turn owns. */
  request: Record<string, unknown>;
  error?: string;
  attemptCount: number;
  /** Attempts that died without a clean end — the dead-letter trigger. */
  poisonCount: number;
  deadLetteredAt?: string;
  deadLetterReason?: string;
}

export interface CreateRunInput {
  runId: string;
  chatId: string;
  scopeId: string;
  request: Record<string, unknown>;
  priority?: number;
  availableAt?: string;
}

/** Fields a transition may write alongside the status change, atomically. */
export interface RunPatch {
  startedAt?: string;
  finishedAt?: string;
  error?: string;
  availableAt?: string;
  priority?: number;
  poisonCount?: number;
  request?: Record<string, unknown>;
}

export interface AttemptRecord {
  attemptId: string;
  runId: string;
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
  runId: string;
  ownerId: string;
}

export interface EndAttemptInput {
  attemptId: string;
  status: Exclude<AttemptStatus, "running">;
  error?: string;
}

/**
 * Exclusive, expiring ownership of a run.
 *
 * `leaseToken` proves "I am the current owner" on every write. `fencingToken` is
 * monotonic across ALL leases, so a resumed-from-the-dead worker holding an old
 * lease can be rejected by comparison even if it somehow re-acquires: the
 * classic fix for the process that paused for a minute and woke up believing it
 * still owned the run.
 */
export interface Lease {
  runId: string;
  attemptId: string;
  ownerId: string;
  leaseToken: string;
  fencingToken: number;
  expiresAt: string;
}

export interface AcquireLeaseInput {
  runId: string;
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
  /** Scopes with a run already executing; the claim must skip them. */
  scopesBusy: string[];
}

export interface ClaimedRun {
  run: RunRecord;
  attempt: AttemptRecord;
  lease: Lease;
}

/**
 * Durable run state: lifecycle, attempts, leases, and the event log.
 *
 * The event log is the product of a run — everything a UI replays and everything
 * a crash recovery reads — so its two invariants are contractual, not advisory
 * (see {@link RunStore.appendEvents}).
 */
export interface RunStore {
  createRun(input: CreateRunInput): Promise<RunRecord>;
  getRun(runId: string): Promise<RunRecord | null>;

  /**
   * Compare-and-set the run's status.
   *
   * MUST reject with {@link InvalidRunTransitionError} when the current status
   * is not in `from` (someone else moved it first — a lost race, not a retryable
   * hiccup) or when `from → to` is not in {@link RUN_TRANSITIONS}. Passing the
   * expected `from` set rather than blind-writing is what makes cancel-vs-finish
   * races resolve to exactly one winner.
   */
  transitionRun(
    runId: string,
    from: RunStatus[],
    to: RunStatus,
    patch?: RunPatch,
  ): Promise<RunRecord>;

  createAttempt(input: CreateAttemptInput): Promise<AttemptRecord>;
  endAttempt(input: EndAttemptInput): Promise<AttemptRecord>;

  acquireLease(input: AcquireLeaseInput): Promise<Lease>;
  /** MUST throw {@link LeaseLostError} when the token is no longer current. */
  renewLease(leaseToken: string, ttlMs: number): Promise<Lease>;
  releaseLease(leaseToken: string): Promise<void>;
  /** Ends leases past their expiry and returns them, so recovery can act. */
  expireStaleLeases(now: Date): Promise<Lease[]>;

  /**
   * Append events to the run's log.
   *
   * Contract — an implementation MUST:
   * - reject a stale `leaseToken` with {@link LeaseLostError}, so a zombie
   *   worker cannot interleave its events with the live attempt's;
   * - reject non-monotonic `seq` with {@link SeqConflictError} — a repeat or a
   *   gap breaks the consumer's only means of detecting a dropped event.
   *
   * It MUST NOT re-stamp `seq`: core owns numbering (`RunChatInput.firstSeq`), so
   * a store that renumbered would desync the ids already streamed to a client.
   */
  appendEvents(
    runId: string,
    events: AiRunEvent[],
    opts: AppendEventsOptions,
  ): Promise<void>;

  listEvents(runId: string, opts?: ListEventsOptions): Promise<AiRunEvent[]>;

  /**
   * The `seq` the next appended event must carry (0 for an empty run). A retry
   * passes it to `runChat` as `firstSeq` so several provider passes produce ONE
   * unbroken sequence under one run id.
   */
  nextSeq(runId: string): Promise<number>;

  /**
   * Atomically claim the highest-priority claimable run whose scope is free,
   * creating its attempt and lease. Returns null when there is nothing to do.
   */
  claimNext(input: ClaimNextInput): Promise<ClaimedRun | null>;

  markDeadLettered(runId: string, reason: string): Promise<RunRecord>;
}
