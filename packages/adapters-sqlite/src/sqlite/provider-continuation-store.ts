import {
  decodeProviderContinuation,
  decodeProviderContinuationScope,
  encodeProviderContinuation,
  LeaseLostError,
  providerContinuationScopeMismatch,
  providerContinuationScopesEqual,
  providerContinuationStateMissing,
  validateProviderContinuationAnchors,
  validateProviderContinuationScope,
  type Clock,
  type ProviderContinuationRecord,
  type ProviderContinuationScope,
  type ProviderContinuationStore,
  type RunWriteFence,
} from "@agentkit/host";
import type { SqliteConnection, TxOwner } from "./connection.js";
import { assertRunLease } from "./lease-fence.js";

interface ContinuationRow {
  run_id: string;
  anchor_message_id: string;
  chat_id: string;
  state_json: string;
}

export class SqliteProviderContinuationStore
  implements ProviderContinuationStore
{
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly owner?: TxOwner,
  ) {}

  async bindRun(
    runId: string,
    scope: ProviderContinuationScope,
    fence: RunWriteFence,
  ): Promise<void> {
    const validated = validateProviderContinuationScope(scope);
    return this.conn.whenFree(() => {
      this.assertFence(runId, validated.chatId, fence);
      const bound = this.selectScope(runId);
      if (bound !== null) {
        if (!providerContinuationScopesEqual(bound, validated))
          providerContinuationScopeMismatch();
        return;
      }
      this.conn.run(
        "INSERT INTO provider_run_scopes (run_id, scope_json) VALUES ($runId, $scope)",
        {
          $runId: runId,
          $scope: JSON.stringify(validated),
        },
      );
    }, this.owner);
  }

  async getRunScope(runId: string): Promise<ProviderContinuationScope | null> {
    return this.conn.readCommitted(() => this.selectScope(runId), this.owner);
  }

  async put(
    input: ProviderContinuationRecord,
    fence: RunWriteFence,
  ): Promise<ProviderContinuationRecord> {
    const json = encodeProviderContinuation(input.state);
    const state = decodeProviderContinuation(json);
    const runId = input.runId;
    const anchorMessageId = input.anchorMessageId;
    return this.conn.whenFree(() => {
      this.assertFence(runId, state.scope.chatId, fence);
      const bound = this.selectScope(runId);
      if (
        bound === null ||
        !providerContinuationScopesEqual(bound, state.scope)
      )
        providerContinuationScopeMismatch();
      this.assertAnchor(anchorMessageId, state.scope.chatId, runId);
      this.conn.run(
        `INSERT INTO provider_continuations (run_id, anchor_message_id, chat_id, state_json)
        VALUES ($runId, $anchor, $chatId, $state)
        ON CONFLICT(run_id) DO UPDATE SET anchor_message_id = excluded.anchor_message_id,
          chat_id = excluded.chat_id, state_json = excluded.state_json`,
        {
          $runId: runId,
          $anchor: anchorMessageId,
          $chatId: state.scope.chatId,
          $state: json,
        },
      );
      this.conn.run(
        "UPDATE provider_run_scopes SET state_required = 1 WHERE run_id = $runId",
        { $runId: runId },
      );
      return {
        runId,
        anchorMessageId,
        state: decodeProviderContinuation(json),
      };
    }, this.owner);
  }

  async getByRun(runId: string): Promise<ProviderContinuationRecord | null> {
    return this.conn.readCommitted(() => {
      const row = this.conn.get(
        "SELECT * FROM provider_continuations WHERE run_id = $runId",
        { $runId: runId },
      ) as ContinuationRow | null;
      if (row !== null) return this.decodeRow(row);
      const bound = this.conn.get(
        "SELECT state_required FROM provider_run_scopes WHERE run_id = $runId",
        { $runId: runId },
      ) as { state_required: number } | null;
      if (bound !== null && bound.state_required !== 0)
        providerContinuationStateMissing();
      return null;
    }, this.owner);
  }

  async findByAnchors(
    chatId: string,
    anchorMessageIds: readonly string[],
  ): Promise<ProviderContinuationRecord[]> {
    const anchors = validateProviderContinuationAnchors(
      chatId,
      anchorMessageIds,
    );
    return this.conn.readCommitted(
      () =>
        anchors.flatMap((anchor) => {
          // Per-anchor queries avoid SQLite's driver-dependent placeholder limit.
          const rows = this.conn.all(
            `SELECT * FROM provider_continuations
        WHERE chat_id = $chatId AND anchor_message_id = $anchor ORDER BY run_id`,
            {
              $chatId: chatId,
              $anchor: anchor,
            },
          ) as ContinuationRow[];
          return rows.map((row) => this.decodeRow(row));
        }),
      this.owner,
    );
  }

  private selectScope(runId: string): ProviderContinuationScope | null {
    const row = this.conn.get(
      "SELECT scope_json FROM provider_run_scopes WHERE run_id = $runId",
      { $runId: runId },
    ) as { scope_json: string } | null;
    if (row === null) return null;
    return decodeProviderContinuationScope(row.scope_json);
  }

  private decodeRow(row: ContinuationRow): ProviderContinuationRecord {
    const state = decodeProviderContinuation(row.state_json);
    const bound = this.selectScope(row.run_id);
    if (
      bound === null ||
      row.chat_id !== state.scope.chatId ||
      !providerContinuationScopesEqual(bound, state.scope)
    )
      providerContinuationScopeMismatch();
    this.assertAnchor(row.anchor_message_id, state.scope.chatId, row.run_id);
    return { runId: row.run_id, anchorMessageId: row.anchor_message_id, state };
  }

  private assertAnchor(
    anchorMessageId: string,
    chatId: string,
    runId: string,
  ): void {
    const anchor = this.conn.get(
      "SELECT chat_id, run_id FROM messages WHERE id = $id",
      { $id: anchorMessageId },
    ) as { chat_id: string; run_id: string | null } | null;
    if (anchor?.chat_id !== chatId || anchor.run_id !== runId)
      providerContinuationScopeMismatch();
  }

  private assertFence(
    runId: string,
    chatId: string,
    fence: RunWriteFence,
  ): void {
    if (!fence || fence.taskId !== runId)
      throw new LeaseLostError(
        "Provider continuation does not belong to the fenced run.",
      );
    assertRunLease(this.conn, this.clock, fence);
    const task = this.conn.get(
      "SELECT payload FROM tasks WHERE task_id = $runId",
      { $runId: runId },
    ) as { payload: string } | null;
    if (task === null) providerContinuationScopeMismatch();
    const payload = JSON.parse(task.payload) as { chatId?: unknown };
    if (payload.chatId !== undefined && payload.chatId !== chatId)
      providerContinuationScopeMismatch();
    if (
      this.conn.get("SELECT id FROM chats WHERE id = $id", { $id: chatId }) ===
      null
    )
      providerContinuationScopeMismatch();
  }
}
