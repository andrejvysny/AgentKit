/**
 * Transactional outbox.
 *
 * A run's events are written to the run log in the same transaction as the state
 * they describe; publishing them to the outside world (SSE, a websocket, a
 * message bus) is a separate, retryable step. Without the outbox the host would
 * have to choose between publishing before the commit (announcing work that may
 * roll back) and publishing after it (losing the announcement on a crash).
 */
export interface OutboxRecord {
  id: string;
  /** Routing key: which stream/queue this belongs to. */
  topic: string;
  /** Correlation key, so a consumer can filter one run's traffic. */
  runId?: string;
  payload: Record<string, unknown>;
  createdAt: string;
  /** Not claimable before this instant (retry backoff). */
  availableAt: string;
  attempts: number;
  publishedAt?: string;
  lastError?: string;
}

export interface OutboxAppendInput {
  id?: string;
  topic: string;
  runId?: string;
  payload: Record<string, unknown>;
  availableAt?: string;
}

export interface OutboxClaimInput {
  limit: number;
  now: Date;
  /** Owner of the claim, so two publishers do not double-send. */
  ownerId?: string;
}

export interface OutboxStore {
  /**
   * Persist a record for publication.
   *
   * `availableAt` is NORMALIZED to a UTC ISO instant
   * (`new Date(x).toISOString()`), because the reference adapters compare it as
   * TEXT: an offset-form string (`…T01:30:00-05:00`) sorts before a `Z` string
   * naming a later instant, so an un-normalized value is claimed hours early.
   * An unparsable value is rejected rather than stored.
   */
  enqueue(input: OutboxAppendInput): Promise<OutboxRecord>;
  /**
   * Claim up to `limit` due records, incrementing their attempt counter.
   *
   * BOUNDED BY ATTEMPTS. A record that has been handed out `maxAttempts` times
   * (adapter option, default 10) is never claimed again: it stays in the table
   * with its `attempts` and `lastError` as an inspectable dead letter, rather
   * than being redelivered forever to a consumer that cannot accept it. There
   * is no separate "dead" flag — `attempts` IS the record of how many times the
   * publisher tried, and a cap read off it needs no schema of its own.
   */
  claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]>;
  /** MUST reject an unknown id with {@link RecordNotFoundError}. */
  markPublished(id: string, at: Date): Promise<void>;
  /**
   * Record the failure and schedule the retry. At the attempt cap the retry
   * schedule no longer matters — `claimBatch` will not take the record again —
   * but the error is still recorded, because that string is the only diagnosis
   * of why publication was abandoned.
   *
   * MUST reject an unknown id with {@link RecordNotFoundError}.
   */
  markFailed(id: string, error: string, retryAt: Date): Promise<void>;
  /**
   * Delete records that can never be claimed again and are older than
   * `before` — published ones (compared on `publishedAt`) and attempt-exhausted
   * ones (compared on `createdAt`). Returns how many rows were removed.
   *
   * RETENTION IS A CALLER'S DECISION, which is why this takes an instant
   * instead of running itself on a timer: how long a published event stays
   * readable is a product question (an audit trail, a debugging window), and a
   * store that swept on its own would answer it for every host. Nothing
   * claimable is ever removed, whatever `before` says — a pruner that could
   * delete work still waiting to be published would be a data-loss button.
   */
  prune(before: Date): Promise<number>;
}
