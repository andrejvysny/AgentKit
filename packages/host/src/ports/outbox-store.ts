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
  enqueue(input: OutboxAppendInput): Promise<OutboxRecord>;
  /** Claim up to `limit` due records, incrementing their attempt counter. */
  claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]>;
  markPublished(id: string, at: Date): Promise<void>;
  /** Record the failure and schedule the retry. */
  markFailed(id: string, error: string, retryAt: Date): Promise<void>;
}
