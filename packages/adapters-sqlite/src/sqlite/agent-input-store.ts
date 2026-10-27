import {
  AgentInputConflictError,
  AgentInputTransitionError,
  assertAgentInputTransition,
  RecordNotFoundError,
  validateCreateAgentInput,
  type AgentInputPatch,
  type AgentInputQuery,
  type AgentInputRecord,
  type AgentInputStatus,
  type AgentInputStore,
  type Clock,
  type CreateAgentInput,
} from "@agentkit/host";
import type { SqliteConnection, TxOwner } from "./connection.js";
import type { SqliteDatabase } from "./driver.js";

/** Registered verbatim in migrations.ts `OPTIONAL_TABLES`; do not reformat. */
export const AGENT_INPUTS_TABLE_SQL =
  "CREATE TABLE agent_inputs (input_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, status TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL, input_order INTEGER NOT NULL, target_task_id TEXT, on_settled TEXT, attempt_id TEXT, tool_call_id TEXT, expires_at TEXT, applied_task_id TEXT, applied_message_id TEXT, answer_id TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL)";

const AGENT_INPUTS_INDEX_SQL =
  "CREATE INDEX IF NOT EXISTS idx_agent_inputs_session ON agent_inputs(session_id, status, input_order)";

/** Idempotent; an existing foreign table is left for the open-time integrity check. */
export function ensureAgentInputTables(db: SqliteDatabase): void {
  db.exec(
    AGENT_INPUTS_TABLE_SQL.replace(
      "CREATE TABLE",
      "CREATE TABLE IF NOT EXISTS",
    ),
  );
  db.exec(AGENT_INPUTS_INDEX_SQL);
}

interface InputRow {
  input_id: string;
  session_id: string;
  kind: AgentInputRecord["kind"];
  status: AgentInputStatus;
  content: string;
  content_hash: string;
  input_order: number;
  target_task_id: string | null;
  on_settled: AgentInputRecord["onSettled"] | null;
  attempt_id: string | null;
  tool_call_id: string | null;
  expires_at: string | null;
  applied_task_id: string | null;
  applied_message_id: string | null;
  answer_id: string | null;
  created_at: string;
  updated_at: string;
}

function recordFromRow(row: InputRow): AgentInputRecord {
  const record: AgentInputRecord = {
    inputId: row.input_id,
    sessionId: row.session_id,
    kind: row.kind,
    status: row.status,
    content: JSON.parse(row.content) as unknown,
    contentHash: row.content_hash,
    order: row.input_order,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
  if (row.target_task_id !== null) record.targetTaskId = row.target_task_id;
  if (row.on_settled !== null) record.onSettled = row.on_settled;
  if (row.attempt_id !== null) record.attemptId = row.attempt_id;
  if (row.tool_call_id !== null) record.toolCallId = row.tool_call_id;
  if (row.expires_at !== null) record.expiresAt = row.expires_at;
  if (row.applied_task_id !== null) record.appliedTaskId = row.applied_task_id;
  if (row.applied_message_id !== null)
    record.appliedMessageId = row.applied_message_id;
  if (row.answer_id !== null) record.answerId = row.answer_id;
  return record;
}

export class SqliteAgentInputStore implements AgentInputStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly owner?: TxOwner,
  ) {}

  async createInput(
    input: CreateAgentInput,
  ): Promise<{ record: AgentInputRecord; created: boolean }> {
    validateCreateAgentInput(input);
    const content = JSON.stringify(input.content);
    return this.conn.whenFree(() => {
      const existing = this.select(input.inputId);
      if (existing) return this.existingOrConflict(existing, input);
      const now = this.clock.nowIso();
      const result = this.conn.run(
        `INSERT INTO agent_inputs (
           input_id, session_id, kind, status, content, content_hash,
           input_order, target_task_id, on_settled, attempt_id, tool_call_id,
           expires_at, created_at, updated_at)
         VALUES (
           $inputId, $sessionId, $kind, 'pending', $content, $contentHash,
           (SELECT COALESCE(MAX(input_order), 0) + 1 FROM agent_inputs),
           $targetTaskId, $onSettled, $attemptId, $toolCallId,
           $expiresAt, $now, $now)
         ON CONFLICT(input_id) DO NOTHING`,
        {
          $inputId: input.inputId,
          $sessionId: input.sessionId,
          $kind: input.kind,
          $content: content,
          $contentHash: input.contentHash,
          $targetTaskId: input.targetTaskId ?? null,
          $onSettled: input.onSettled ?? null,
          $attemptId: input.attemptId ?? null,
          $toolCallId: input.toolCallId ?? null,
          $expiresAt: input.expiresAt ?? null,
          $now: now,
        },
      );
      // Another handle inserted the same key between the read and this write:
      // resolve it exactly like the read-side duplicate, never as a raw
      // constraint error.
      if (Number(result.changes) !== 1)
        return this.existingOrConflict(
          this.select(input.inputId) as AgentInputRecord,
          input,
        );
      return {
        record: this.select(input.inputId) as AgentInputRecord,
        created: true,
      };
    }, this.owner);
  }

  /** Idempotent re-create, or the conflict a differing duplicate is. */
  private existingOrConflict(
    existing: AgentInputRecord,
    input: CreateAgentInput,
  ): { record: AgentInputRecord; created: boolean } {
    if (
      existing.kind !== input.kind ||
      existing.sessionId !== input.sessionId ||
      existing.contentHash !== input.contentHash
    ) {
      throw new AgentInputConflictError(
        `Input ${input.inputId} already exists with a different kind, session or content.`,
        { inputId: input.inputId },
      );
    }
    return { record: existing, created: false };
  }

  async getInput(inputId: string): Promise<AgentInputRecord | undefined> {
    return this.conn.readCommitted(() => this.select(inputId), this.owner);
  }

  async listInputs(query: AgentInputQuery): Promise<AgentInputRecord[]> {
    return this.conn.readCommitted(() => {
      const params: Record<string, string> = { $sessionId: query.sessionId };
      const where = ["session_id = $sessionId"];
      const addIn = (
        column: string,
        prefix: string,
        values: readonly string[] | undefined,
      ): void => {
        if (values === undefined) return;
        if (values.length === 0) {
          where.push("0 = 1");
          return;
        }
        const names = values.map((value, index) => {
          params[`$${prefix}${index}`] = value;
          return `$${prefix}${index}`;
        });
        where.push(`${column} IN (${names.join(", ")})`);
      };
      addIn("kind", "kind", query.kinds);
      addIn("status", "status", query.statuses);
      if (query.targetTaskId !== undefined) {
        where.push("target_task_id = $targetTaskId");
        params["$targetTaskId"] = query.targetTaskId;
      }
      return this.conn
        .all(
          `SELECT * FROM agent_inputs WHERE ${where.join(" AND ")} ORDER BY input_order ASC, input_id ASC`,
          params,
        )
        .map((row) => recordFromRow(row as InputRow));
    }, this.owner);
  }

  async transitionInput(
    inputId: string,
    from: readonly AgentInputStatus[],
    to: AgentInputStatus,
    patch?: AgentInputPatch,
  ): Promise<AgentInputRecord> {
    return this.conn.whenFree(() => {
      const current = this.select(inputId);
      if (!current)
        throw new RecordNotFoundError(`Input not found: ${inputId}`);
      assertAgentInputTransition(current, from, to);
      const result = this.conn.run(
        `UPDATE agent_inputs SET
           status = $to,
           applied_task_id = COALESCE($appliedTaskId, applied_task_id),
           applied_message_id = COALESCE($appliedMessageId, applied_message_id),
           answer_id = COALESCE($answerId, answer_id),
           updated_at = $now
         WHERE input_id = $inputId AND status = $current`,
        {
          $to: to,
          $appliedTaskId: patch?.appliedTaskId ?? null,
          $appliedMessageId: patch?.appliedMessageId ?? null,
          $answerId: patch?.answerId ?? null,
          $now: this.clock.nowIso(),
          $inputId: inputId,
          $current: current.status,
        },
      );
      // The WHERE re-checks the status the guard above saw: another handle can
      // move the row between that read and this write, and a CAS that reports
      // success without having written is exactly the double placement the
      // store exists to rule out.
      if (Number(result.changes) !== 1) {
        throw new AgentInputTransitionError(
          `Input ${inputId} changed concurrently; expected ${current.status}.`,
          { inputId, from, to },
        );
      }
      return this.select(inputId) as AgentInputRecord;
    }, this.owner);
  }

  private select(inputId: string): AgentInputRecord | undefined {
    const row = this.conn.get(
      "SELECT * FROM agent_inputs WHERE input_id = $inputId",
      { $inputId: inputId },
    ) as InputRow | null;
    return row ? recordFromRow(row) : undefined;
  }
}
