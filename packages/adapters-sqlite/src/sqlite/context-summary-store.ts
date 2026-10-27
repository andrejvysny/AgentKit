import type { ContextSummaryRecord, ContextSummaryStore } from "@agentkit/host";
import type { SqliteConnection, TxOwner } from "./connection.js";
import type { SqliteDatabase } from "./driver.js";

/** Registered verbatim in migrations.ts `OPTIONAL_TABLES`; do not reformat. */
export const CONTEXT_SUMMARIES_TABLE_SQL =
  "CREATE TABLE context_summaries (summary_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, covers_through_message_id TEXT NOT NULL, first_kept_message_id TEXT NOT NULL, summary TEXT NOT NULL, source_hash TEXT NOT NULL, tokens_before INTEGER NOT NULL, tokens_after INTEGER NOT NULL, provider_id TEXT, model TEXT NOT NULL, summarizer_revision TEXT NOT NULL, details TEXT NOT NULL, created_at TEXT NOT NULL)";

const CONTEXT_SUMMARIES_INDEX_SQL =
  "CREATE INDEX IF NOT EXISTS idx_context_summaries_session ON context_summaries(session_id, created_at)";

/** Idempotent; an existing foreign table is left for the open-time integrity check. */
export function ensureContextSummaryTables(db: SqliteDatabase): void {
  db.exec(
    CONTEXT_SUMMARIES_TABLE_SQL.replace(
      "CREATE TABLE",
      "CREATE TABLE IF NOT EXISTS",
    ),
  );
  db.exec(CONTEXT_SUMMARIES_INDEX_SQL);
}

interface SummaryRow {
  summary_id: string;
  session_id: string;
  covers_through_message_id: string;
  first_kept_message_id: string;
  summary: string;
  source_hash: string;
  tokens_before: number;
  tokens_after: number;
  provider_id: string | null;
  model: string;
  summarizer_revision: string;
  details: string;
  created_at: string;
}

function recordFromRow(row: SummaryRow): ContextSummaryRecord {
  const record: ContextSummaryRecord = {
    summaryId: row.summary_id,
    sessionId: row.session_id,
    coversThroughMessageId: row.covers_through_message_id,
    firstKeptMessageId: row.first_kept_message_id,
    summary: row.summary,
    sourceHash: row.source_hash,
    tokensBefore: row.tokens_before,
    tokensAfter: row.tokens_after,
    model: row.model,
    summarizerRevision: row.summarizer_revision,
    details: JSON.parse(row.details) as ContextSummaryRecord["details"],
    createdAt: row.created_at,
  };
  if (row.provider_id !== null) record.providerId = row.provider_id;
  return record;
}

export class SqliteContextSummaryStore implements ContextSummaryStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly owner?: TxOwner,
  ) {}

  async saveSummary(record: ContextSummaryRecord): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO context_summaries (
           summary_id, session_id, covers_through_message_id,
           first_kept_message_id, summary, source_hash, tokens_before,
           tokens_after, provider_id, model, summarizer_revision, details,
           created_at)
         VALUES (
           $summaryId, $sessionId, $coversThrough, $firstKept, $summary,
           $sourceHash, $tokensBefore, $tokensAfter, $providerId, $model,
           $revision, $details, $createdAt)
         ON CONFLICT(summary_id) DO NOTHING`,
        {
          $summaryId: record.summaryId,
          $sessionId: record.sessionId,
          $coversThrough: record.coversThroughMessageId,
          $firstKept: record.firstKeptMessageId,
          $summary: record.summary,
          $sourceHash: record.sourceHash,
          $tokensBefore: record.tokensBefore,
          $tokensAfter: record.tokensAfter,
          $providerId: record.providerId ?? null,
          $model: record.model,
          $revision: record.summarizerRevision,
          $details: JSON.stringify(record.details),
          $createdAt: record.createdAt,
        },
      );
    }, this.owner);
  }

  async listSummaries(sessionId: string): Promise<ContextSummaryRecord[]> {
    return this.conn.readCommitted(
      () =>
        this.conn
          .all(
            "SELECT * FROM context_summaries WHERE session_id = $sessionId ORDER BY created_at DESC, rowid DESC",
            { $sessionId: sessionId },
          )
          .map((row) => recordFromRow(row as SummaryRow)),
      this.owner,
    );
  }

  async deleteSessionSummaries(sessionId: string): Promise<number> {
    return this.conn.whenFree(() => {
      const result = this.conn.run(
        "DELETE FROM context_summaries WHERE session_id = $sessionId",
        { $sessionId: sessionId },
      );
      return Number(result.changes);
    }, this.owner);
  }
}
