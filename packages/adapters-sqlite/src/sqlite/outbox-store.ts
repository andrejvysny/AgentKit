/**
 * `bun:sqlite`-backed {@link OutboxStore}: at-least-once delivery with a
 * visibility-window claim lease and an attempt cap.
 *
 * Split out of `sqlite-assistant-store.ts` — one sub-store per file, sharing
 * {@link SqliteConnection} and the row mappers in `rows.js`.
 */
import type { Changes } from "bun:sqlite";
import {
  type Clock,
  type OutboxAppendInput,
  type OutboxClaimInput,
  type OutboxRecord,
  type OutboxStore,
  RecordNotFoundError,
} from "@agentkit/host";
import type { SqliteConnection, TxOwner } from "./connection.js";
import {
  normalizeInstant,
  type OutboxRow,
  outboxFromRow,
  toJson,
} from "./rows.js";

const DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS = 30_000;
/**
 * How many times one outbox record may be handed to a publisher before the
 * queue stops offering it. Ten is generous for a transient consumer outage
 * (with the caller's own backoff between them) and short of "forever", which is
 * what an uncapped outbox meant: a payload no consumer can accept was
 * redelivered on every claim for the life of the database.
 */
const DEFAULT_OUTBOX_MAX_ATTEMPTS = 10;

export class SqliteOutboxStore implements OutboxStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly claimVisibilityMs: number = DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS,
    private readonly maxAttempts: number = DEFAULT_OUTBOX_MAX_ATTEMPTS,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async enqueue(input: OutboxAppendInput): Promise<OutboxRecord> {
    const now = this.clock.nowIso();
    const id = input.id ?? `outbox_${crypto.randomUUID()}`;
    // Normalized because `claimBatch` compares this column as TEXT — see
    // `normalizeInstant`.
    const availableAt =
      input.availableAt === undefined
        ? now
        : normalizeInstant(input.availableAt, "availableAt");
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO outbox (id, topic, run_id, payload, created_at, available_at, attempts)
         VALUES ($id, $topic, $runId, $payload, $now, $availableAt, 0)`,
        {
          $id: id,
          $topic: input.topic,
          $runId: input.runId ?? null,
          $payload: toJson(input.payload),
          $now: now,
          $availableAt: availableAt,
        },
      );
    }, this.txOwner);
    return {
      id,
      topic: input.topic,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      payload: input.payload,
      createdAt: now,
      availableAt,
      attempts: 0,
    };
  }

  async claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]> {
    return this.conn.whenFree(() => {
      const nowIso = input.now.toISOString();
      // `attempts < $maxAttempts` is the cap, and it needs no column of its
      // own: `attempts` already counts deliveries, so a record that has used
      // its budget simply stops matching the claim query and stays behind as an
      // inspectable dead letter.
      const rows = this.conn.all(
        `SELECT * FROM outbox
          WHERE published_at IS NULL AND available_at <= $now AND attempts < $maxAttempts
          ORDER BY available_at ASC, rowid ASC LIMIT $limit`,
        { $now: nowIso, $limit: input.limit, $maxAttempts: this.maxAttempts },
      ) as OutboxRow[];
      if (rows.length === 0) return [];
      // Push the visibility window forward so a concurrent claimBatch call
      // does not hand the same in-flight record to a second publisher before
      // markPublished/markFailed resolves it — the port has no separate
      // "claimed" flag, so available_at doubles as the claim lease.
      const newAvailableAt = new Date(
        input.now.getTime() + this.claimVisibilityMs,
      ).toISOString();
      const claimed: OutboxRecord[] = [];
      for (const row of rows) {
        const attempts = row.attempts + 1;
        this.conn.run(
          `UPDATE outbox SET attempts = $attempts, available_at = $availableAt WHERE id = $id`,
          { $attempts: attempts, $availableAt: newAvailableAt, $id: row.id },
        );
        claimed.push(
          outboxFromRow({ ...row, attempts, available_at: newAvailableAt }),
        );
      }
      return claimed;
    }, this.txOwner);
  }

  async markPublished(id: string, at: Date): Promise<void> {
    await this.conn.whenFree(() => {
      const result = this.conn.run(
        `UPDATE outbox SET published_at = $at WHERE id = $id`,
        { $at: at.toISOString(), $id: id },
      );
      assertOutboxRowTouched(result, id);
    }, this.txOwner);
  }

  async markFailed(id: string, error: string, retryAt: Date): Promise<void> {
    await this.conn.whenFree(() => {
      const result = this.conn.run(
        `UPDATE outbox SET last_error = $error, available_at = $retryAt WHERE id = $id`,
        { $error: error, $retryAt: retryAt.toISOString(), $id: id },
      );
      assertOutboxRowTouched(result, id);
    }, this.txOwner);
  }

  /**
   * Drop what can never be claimed again: published records older than
   * `before`, and records that used up their attempt budget before it.
   *
   * The two halves compare DIFFERENT columns on purpose. A published record is
   * retained for as long as someone might want to read what was sent, which is
   * measured from when it was sent (`published_at`); an exhausted one was never
   * sent at all, so the only age it has is its own (`created_at`).
   */
  async prune(before: Date): Promise<number> {
    return this.conn.whenFree(() => {
      const beforeIso = before.toISOString();
      return this.conn.run(
        `DELETE FROM outbox
          WHERE (published_at IS NOT NULL AND published_at < $before)
             OR (published_at IS NULL AND attempts >= $maxAttempts AND created_at < $before)`,
        { $before: beforeIso, $maxAttempts: this.maxAttempts },
      ).changes;
    }, this.txOwner);
  }
}

/**
 * A `markPublished`/`markFailed` that matched no row is a caller naming an id
 * this store does not have — a publisher resolving a record someone pruned, or
 * a plain typo. It used to be a silent no-op, which made "the publisher says it
 * published, the row says it did not" a mystery with no error anywhere.
 */
function assertOutboxRowTouched(result: Changes, id: string): void {
  if (result.changes === 0) {
    throw new RecordNotFoundError(`Outbox record not found: ${id}`, { id });
  }
}
