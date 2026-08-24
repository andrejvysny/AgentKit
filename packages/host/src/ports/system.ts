/**
 * Ambient system capabilities every other port and service takes as a
 * dependency: time, identity, and logging.
 *
 * They are ports rather than direct calls to `Date`/`crypto`/`console` because
 * an orchestrator's correctness is defined in terms of them — lease expiry,
 * idempotency keys, ordering — and a test that cannot move the clock or pin an
 * id cannot assert any of it.
 */

/** Wall-clock access. The only source of "now" the host layer is allowed. */
export interface Clock {
  now(): Date;
  /** ISO-8601 rendering of {@link now}; what every record persists. */
  nowIso(): string;
}

/**
 * Identifier minting. Split per entity (rather than one `id()`) so a fake can
 * hand out readable, per-kind sequences (`task-1`, `op-1`) and a test asserting
 * "the operation id was reused" reads as intent, not coincidence.
 */
export interface IdGenerator {
  taskId(): string;
  attemptId(): string;
  eventId(): string;
  proposalId(): string;
  /**
   * Identifies ONE apply attempt of a proposal. The idempotency key for the
   * side-effecting half of the system: replaying an apply with the same
   * operation id must return the recorded outcome, never re-run the work.
   */
  operationId(): string;
  messageId(): string;
}

/** Structured logging. `fields` is a flat bag, not a formatted string. */
export interface Logger {
  debug(message: string, fields?: Record<string, unknown>): void;
  info(message: string, fields?: Record<string, unknown>): void;
  warn(message: string, fields?: Record<string, unknown>): void;
  error(message: string, fields?: Record<string, unknown>): void;
}

/**
 * The obvious {@link Clock}. Provided here because every consumer needs one and
 * a host should not have to write four lines of boilerplate to say "use the
 * system clock".
 */
export const defaultClock: Clock = {
  now: () => new Date(),
  nowIso: () => new Date().toISOString(),
};

/**
 * UUID-backed {@link IdGenerator}. Prefixed per kind so an id in a log line or
 * a database row says what it identifies without a join.
 *
 * `crypto.randomUUID` is a web-standard global (Bun, Node ≥ 19, browsers) — the
 * host layer deliberately imports nothing from `node:*`.
 */
export const defaultIds: IdGenerator = {
  taskId: () => `task_${crypto.randomUUID()}`,
  attemptId: () => `att_${crypto.randomUUID()}`,
  eventId: () => `evt_${crypto.randomUUID()}`,
  proposalId: () => `prp_${crypto.randomUUID()}`,
  operationId: () => `op_${crypto.randomUUID()}`,
  messageId: () => `msg_${crypto.randomUUID()}`,
};
