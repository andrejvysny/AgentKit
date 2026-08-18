/**
 * Host-layer error vocabulary.
 *
 * Every error the host raises carries a stable machine-readable `code`, because
 * a durable orchestrator's failures are consumed by other code (a task runner
 * deciding retry vs dead-letter, a tool deciding "blocked" vs "fresh attempt"),
 * not only by a human reading a message. Matching on message text is how retry
 * logic silently rots; matching on `code` does not.
 */
export class AgentKitHostError extends Error {
  readonly code: string;
  readonly details?: Record<string, unknown>;

  constructor(
    code: string,
    message: string,
    details?: Record<string, unknown>,
  ) {
    super(message);
    // `new.target.name` keeps the subclass name on the instance without every
    // subclass repeating a `this.name = ...` line.
    this.name = new.target.name;
    this.code = code;
    if (details !== undefined) this.details = details;
  }
}

/**
 * A run status change that the {@link RUN_TRANSITIONS} table forbids, or whose
 * `from` set did not include the run's current status (a lost race: someone
 * else moved the run first).
 */
export class InvalidRunTransitionError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("invalid_run_transition", message, details);
  }
}

/**
 * A proposal status change the {@link PROPOSAL_TRANSITIONS} table forbids.
 * Same-state "transitions" are illegal too: re-entering `applying` would mean a
 * second apply of side effects that already happened.
 */
export class InvalidProposalTransitionError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("invalid_proposal_transition", message, details);
  }
}

/**
 * The lease this writer holds is no longer the current one — it expired and was
 * taken over, or was released. The writer MUST stop: a second worker owns the
 * run now, and appending would interleave two attempts into one event stream.
 */
export class LeaseLostError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("lease_lost", message, details);
  }
}

/**
 * An event batch whose `seq` numbers do not continue the run's stream (a repeat
 * or a gap). Rejecting is the point: `seq` is the consumer's ordering and
 * gap-detection key, so a store that quietly accepted a duplicate would make
 * "did I miss an event?" unanswerable.
 */
export class SeqConflictError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("seq_conflict", message, details);
  }
}

/**
 * A proposal was created with an `actionId` already used in the same scope.
 * `(scopeKey, actionId)` is the idempotency key for model-issued writes, so the
 * uniqueness violation is the guard doing its job, not a storage accident.
 */
export class DuplicateActionIdError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("duplicate_action_id", message, details);
  }
}

/**
 * The scope moved on since the proposal was built: what the model reviewed is
 * not what would be written. The proposal is invalidated rather than applied —
 * a stale write is worse than no write.
 */
export class RevisionConflictError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("revision_conflict", message, details);
  }
}

/** A record the caller referenced by id does not exist (or is out of scope). */
export class RecordNotFoundError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("not_found", message, details);
  }
}
