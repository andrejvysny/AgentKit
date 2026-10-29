import {
  AgentKitHostError,
  RecordNotFoundError,
  readToolExecutionRecord,
  type Clock,
  type RunWriteFence,
  type ToolExecutionAdmission,
  type ToolExecutionRecord,
  type ToolExecutionStore,
} from "@agentkit/host";
import type { AiToolResult } from "@agentkit/contracts";
import type { SqliteConnection, TxOwner } from "./connection.js";
import type { SqliteDatabase } from "./driver.js";
import { assertRunLease } from "./lease-fence.js";

export const TOOL_EXECUTIONS_TABLE_SQL =
  "CREATE TABLE tool_executions (execution_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE CASCADE, approval_request_id TEXT UNIQUE, record TEXT NOT NULL)";

export function ensureToolExecutionTables(db: SqliteDatabase): void {
  db.exec(
    TOOL_EXECUTIONS_TABLE_SQL.replace(
      "CREATE TABLE",
      "CREATE TABLE IF NOT EXISTS",
    ),
  );
  db.exec(
    "CREATE INDEX IF NOT EXISTS idx_tool_executions_run ON tool_executions(run_id)",
  );
}

export class SqliteToolExecutionStore implements ToolExecutionStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly owner?: TxOwner,
  ) {}

  async admit(
    input: ToolExecutionAdmission,
    fence: RunWriteFence,
  ): Promise<boolean> {
    readToolExecutionRecord({ ...input, status: "admitted" });
    return this.conn.whenFree(() => {
      assertRunLease(this.conn, this.clock, fence);
      if (input.runId !== fence.taskId)
        throw new RecordNotFoundError(
          "Execution does not belong to the fenced run.",
        );
      const records = this.selectRun(input.runId);
      const previous = records
        .filter(
          (record) =>
            record.toolName === input.toolName &&
            record.argsSha256 === input.argsSha256 &&
            record.contextSha256 === input.contextSha256,
        )
        .at(-1);
      if (
        previous?.status === "admitted" &&
        input.approvalGeneration !== previous.executionId
      )
        return false;
      return (
        this.conn.run(
          "INSERT INTO tool_executions (execution_id, run_id, approval_request_id, record) VALUES ($id, $run, $approval, $record) ON CONFLICT DO NOTHING",
          {
            $id: input.executionId,
            $run: input.runId,
            $approval: input.approvalRequestId ?? null,
            $record: JSON.stringify({ ...input, status: "admitted" }),
          },
        ).changes === 1
      );
    }, this.owner);
  }

  async complete(
    executionId: string,
    result: AiToolResult<unknown>,
    fence: RunWriteFence,
  ): Promise<void> {
    return this.conn.whenFree(() => {
      assertRunLease(this.conn, this.clock, fence);
      const record = this.selectRun(fence.taskId).find(
        (receipt) => receipt.executionId === executionId,
      );
      if (!record)
        throw new RecordNotFoundError(
          "Execution does not belong to the fenced run.",
        );
      if (record.status === "completed") return;
      const serialized = JSON.stringify({
        ...record,
        status: "completed",
        result,
      });
      readToolExecutionRecord(JSON.parse(serialized) as unknown);
      this.conn.run(
        "UPDATE tool_executions SET record = $record WHERE execution_id = $id",
        { $id: executionId, $record: serialized },
      );
    }, this.owner);
  }

  async listByRun(runId: string): Promise<ToolExecutionRecord[]> {
    return this.conn.whenFree(() => this.selectRun(runId), this.owner);
  }

  private selectRun(runId: string): ToolExecutionRecord[] {
    try {
      return (
        this.conn.all(
          "SELECT execution_id, run_id, approval_request_id, record FROM tool_executions WHERE run_id = $run ORDER BY rowid",
          { $run: runId },
        ) as {
          execution_id: string;
          run_id: string;
          approval_request_id: string | null;
          record: string;
        }[]
      ).map((row) =>
        readToolExecutionRecord(JSON.parse(row.record) as unknown, {
          executionId: row.execution_id,
          runId: row.run_id,
          approvalRequestId: row.approval_request_id,
        }),
      );
    } catch {
      throw new AgentKitHostError(
        "recovery_history_unreadable",
        "Cannot read durable execution receipts; mutations remain blocked.",
      );
    }
  }
}
