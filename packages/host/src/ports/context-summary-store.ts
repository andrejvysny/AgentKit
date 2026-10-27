/** Version stamped on every summary; a changed prompt or format bumps it. */
export const COMPACTION_SUMMARIZER_REVISION = "agentkit-compaction-v1";

export interface ContextSummaryRecord {
  summaryId: string;
  /** The chat id. */
  sessionId: string;
  /** Last canonical record covered (inclusive). */
  coversThroughMessageId: string;
  /** First record NOT covered (start of the kept tail). */
  firstKeptMessageId: string;
  summary: string;
  /** sha256 hex over the covered records' (id, role, content, toolCallId), in order. */
  sourceHash: string;
  tokensBefore: number;
  tokensAfter: number;
  providerId?: string;
  model: string;
  summarizerRevision: string;
  details: { readFiles: string[]; modifiedFiles: string[] };
  createdAt: string;
}

/**
 * Derived, versioned summaries of a session's older history (ADR 0017 part 2).
 *
 * A summary never replaces canonical records: it is a cache a turn may use
 * while it still matches the path it was made from, and ignores when it does
 * not (branch switch, fork, edit). Optional on {@link AssistantStore} so
 * third-party stores stay compatible.
 */
export interface ContextSummaryStore {
  /** Idempotent on `summaryId`: a repeat save leaves the first record. */
  saveSummary(record: ContextSummaryRecord): Promise<void>;
  /** Newest first. */
  listSummaries(sessionId: string): Promise<ContextSummaryRecord[]>;
  /** Remove every summary of a session; returns how many rows went. */
  deleteSessionSummaries(sessionId: string): Promise<number>;
}
