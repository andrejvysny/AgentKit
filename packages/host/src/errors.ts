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
 * Base for the named subclasses below, whose `code` is part of the closed
 * {@link HostErrorCode} union.
 *
 * `AgentKitHostError` itself keeps `code: string` — other host modules throw
 * it directly with ad hoc codes (`no_model`, `invalid_decision`, …) that are
 * not part of this stable, documented vocabulary, and typing the base
 * constructor to the union would break those call sites. This intermediate
 * class exists so THIS file cannot drift instead: a subclass's `super(code, …)`
 * literal is checked against {@link HostErrorCode}, so adding a subclass here
 * without adding its code to {@link HOST_ERROR_CODES} fails to compile.
 */
abstract class NamedHostError extends AgentKitHostError {
  // biome-ignore lint/complexity/noUselessConstructor: not useless — narrows `code` to HostErrorCode; removing it silently widens every subclass's code param back to string.
  constructor(
    code: HostErrorCode,
    message: string,
    details?: Record<string, unknown>,
  ) {
    super(code, message, details);
  }
}

/**
 * A task status change that the {@link TASK_TRANSITIONS} table forbids, or whose
 * `from` set did not include the task's current status (a lost race: someone
 * else moved the task first).
 */
export class InvalidTaskTransitionError extends NamedHostError {
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
export class DuplicateTaskError extends NamedHostError {
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
export class ExecutorNotFoundError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("executor_not_found", message, details);
  }
}

/**
 * A proposal status change the {@link PROPOSAL_TRANSITIONS} table forbids.
 * Same-state "transitions" are illegal too: re-entering `applying` would mean a
 * second apply of side effects that already happened.
 */
export class InvalidProposalTransitionError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("invalid_proposal_transition", message, details);
  }
}

/**
 * The lease this writer holds is no longer the current one — it expired and was
 * taken over, or was released. The writer MUST stop: a second worker owns the
 * run now, and appending would interleave two attempts into one event stream.
 */
export class LeaseLostError extends NamedHostError {
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
export class SeqConflictError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("seq_conflict", message, details);
  }
}

/**
 * A proposal was created with an `actionId` already used in the same scope.
 * `(scopeKey, actionId)` is the idempotency key for model-issued writes, so the
 * uniqueness violation is the guard doing its job, not a storage accident.
 */
export class DuplicateActionIdError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("duplicate_action_id", message, details);
  }
}

/**
 * The scope moved on since the proposal was built: what the model reviewed is
 * not what would be written. The proposal is invalidated rather than applied —
 * a stale write is worse than no write.
 */
export class RevisionConflictError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("revision_conflict", message, details);
  }
}

/**
 * A chat fork was asked to start from a message that is not a legal fork point:
 * unknown, in a different chat, or on a branch the chat is not currently showing.
 *
 * All three are the same mistake from the store's side — the caller named a
 * point the conversation does not currently pass through — and all three are
 * refused rather than reinterpreted. Forking from the nearest active ancestor
 * instead would silently hand back a conversation the caller never asked for,
 * and the caller has no way to notice. Terminal by classification: the fork
 * point will not wander onto the active path on a retry.
 */
export class InvalidForkPointError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("invalid_fork_point", message, details);
  }
}

/**
 * The {@link UsageAuthorizer} refused a provider call before it was made.
 *
 * Raised by `TurnRunner` on the pass that was refused, so the model is never
 * reached: the whole point of asking before spending is that a "no" costs
 * nothing. Terminal for THIS attempt and reported as such — the run fails with
 * a `run.failed` event carrying `errorCode: "usage_denied"` — but not
 * necessarily terminal for the caller: `UsageAuthorizationDecision.retryAfterMs`
 * exists precisely because a refilling quota says yes later, and the transport
 * maps this to a 429 rather than a 4xx that tells a client to stop asking.
 */
export class UsageDeniedError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("usage_denied", message, details);
  }
}

/** A record the caller referenced by id does not exist (or is out of scope). */
export class RecordNotFoundError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("not_found", message, details);
  }
}

/**
 * A task was created naming a `dependsOn` id or a `parentTaskId` the store does
 * not have — or naming itself as its own dependency.
 *
 * This is what makes the dependency graph acyclic BY CONSTRUCTION: a dependency
 * must already exist when the dependent is written, so an edge can only ever
 * point backwards in creation order and no cycle can be expressed. The
 * alternative — accepting the edge and detecting cycles later — means a queue
 * that can deadlock on data it already committed, discovered by whoever is
 * on-call. Terminal by classification: the referenced id will not appear
 * retroactively.
 */
export class UnknownDependencyError extends NamedHostError {
  constructor(message: string, details?: Record<string, unknown>) {
    super("unknown_dependency", message, details);
  }
}

/**
 * The closed set of codes the named error classes above use — every literal
 * passed to a `super(...)` call in this file, and nothing else.
 *
 * This is deliberately NOT every code `AgentKitHostError` is ever constructed
 * with: other host modules throw the base class directly with situational
 * codes (`no_model`, `invalid_decision`, `task_not_executable`,
 * `invalid_append`, …) that are not part of this stable, cross-package
 * vocabulary. Consumers outside the
 * host package (`@agentkit/transport-http`'s status table) map against THIS
 * list; unrecognized codes fall back to a generic status rather than being
 * silently misclassified.
 *
 * Keep in sync with the classes above by construction, not by discipline:
 * {@link NamedHostError}'s constructor types `code` as {@link HostErrorCode},
 * so a new subclass with a code missing from this tuple fails to compile
 * (see `tests/errors.test.ts` for the belt-and-suspenders repo-scan that also
 * catches it at test time).
 */
export const HOST_ERROR_CODES = [
  "invalid_task_transition",
  "duplicate_task",
  "executor_not_found",
  "invalid_proposal_transition",
  "lease_lost",
  "seq_conflict",
  "duplicate_action_id",
  "revision_conflict",
  "not_found",
  "invalid_fork_point",
  "unknown_dependency",
  "usage_denied",
] as const;

export type HostErrorCode = (typeof HOST_ERROR_CODES)[number];
