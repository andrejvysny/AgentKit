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
 * A task status change that the {@link TASK_TRANSITIONS} table forbids, or whose
 * `from` set did not include the task's current status (a lost race: someone
 * else moved the task first).
 */
export class InvalidTaskTransitionError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("invalid_task_transition", message, details);
  }
}

/**
 * A task was created with a `taskId` that already exists.
 *
 * The id is the caller's idempotency key — a retried submit, a re-delivered
 * message — so a store that silently overwrote would discard a live task's
 * payload and attempt history while its event log stayed behind. Failing loudly
 * lets the caller decide: `TaskService.submitTask` treats it as "already
 * submitted" and re-pokes the queue instead of writing a second task.
 */
export class DuplicateTaskError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("duplicate_task", message, details);
  }
}

/**
 * A task was claimed whose `kind` no executor in this worker's registry
 * handles — a kind that was never registered, or a deployment that routed the
 * work to the wrong process.
 *
 * `details.kind` carries the offending kind, because that is the only thing a
 * human fixing this needs and the only thing a metric should group on. Terminal
 * by classification: retrying cannot conjure the executor, and re-running the
 * same claim would just burn the attempt budget on the same lookup.
 */
export class ExecutorNotFoundError extends AgentKitHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("executor_not_found", message, details);
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
