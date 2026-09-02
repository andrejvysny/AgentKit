/**
 * `bun:sqlite`-backed {@link ProposalStore}: proposal CAS transitions,
 * apply-outcome recording, and revision-based invalidation.
 *
 * Split out of `sqlite-assistant-store.ts` — one sub-store per file, sharing
 * {@link SqliteConnection} and the row mappers in `rows.js`.
 */
import {
  assertProposalTransition,
  DuplicateActionIdError,
  InvalidProposalTransitionError,
  RecordNotFoundError,
  type ApplyOutcome,
  type Clock,
  type CreateProposalInput,
  type ListProposalsOptions,
  type ProposalPatch,
  type ProposalRecord,
  type ProposalStatus,
  type ProposalStore,
} from "@agentkit/host";
import type { Params, SqliteConnection, TxOwner } from "./connection.js";
import {
  isConstraintError,
  outcomeFromRow,
  type ProposalOutcomeRow,
  proposalFromRow,
  type ProposalRow,
  toIntBool,
  toJson,
} from "./rows.js";

export class SqliteProposalStore implements ProposalStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async create(input: CreateProposalInput): Promise<ProposalRecord> {
    return this.conn.whenFree(() => {
      try {
        this.conn.run(
          `INSERT INTO proposals
             (id, chat_id, run_id, scope_key, action_id, tool_name, kind, risk, status,
              envelope, operations, warnings, truncated, revision_at_create, created_at)
           VALUES
             ($id, $chatId, $runId, $scopeKey, $actionId, $toolName, $kind, $risk, 'pending',
              $envelope, $operations, $warnings, $truncated, $revisionAtCreate, $createdAt)`,
          {
            $id: input.id,
            $chatId: input.chatId,
            $runId: input.runId ?? null,
            $scopeKey: input.scopeKey,
            $actionId: input.actionId ?? null,
            $toolName: input.toolName,
            $kind: input.kind,
            $risk: input.risk,
            $envelope: toJson(input.envelope),
            $operations: toJson(input.operations),
            $warnings: toJson(input.warnings),
            $truncated: toIntBool(input.truncated),
            $revisionAtCreate: input.revisionAtCreate ?? null,
            $createdAt: input.createdAt,
          },
        );
      } catch (err) {
        if (isConstraintError(err)) {
          throw new DuplicateActionIdError(
            `action_id ${input.actionId} already used in scope ${input.scopeKey}.`,
            { scopeKey: input.scopeKey, actionId: input.actionId },
          );
        }
        throw err;
      }
      return proposalFromRow(this.selectProposalRow(input.id)!);
    }, this.txOwner);
  }

  async get(proposalId: string): Promise<ProposalRecord | null> {
    const row = this.selectProposalRow(proposalId);
    return row ? proposalFromRow(row) : null;
  }

  async getByActionId(
    scopeKey: string,
    actionId: string,
  ): Promise<ProposalRecord | null> {
    // Most recent wins: rowid increases with insertion order.
    const row = this.conn.get(
      `SELECT * FROM proposals WHERE scope_key = $scopeKey AND action_id = $actionId
       ORDER BY rowid DESC LIMIT 1`,
      { $scopeKey: scopeKey, $actionId: actionId },
    ) as ProposalRow | undefined;
    return row ? proposalFromRow(row) : null;
  }

  async listByChat(
    chatId: string,
    opts?: ListProposalsOptions,
  ): Promise<ProposalRecord[]> {
    let sql = `SELECT * FROM proposals WHERE chat_id = $chatId`;
    const params: Params = { $chatId: chatId };
    if (opts?.status !== undefined) {
      sql += ` AND status = $status`;
      params.$status = opts.status;
    }
    sql += ` ORDER BY rowid ASC`;
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as ProposalRow[];
    return rows.map(proposalFromRow);
  }

  async listByStatus(
    status: ProposalStatus,
    opts?: { limit?: number },
  ): Promise<ProposalRecord[]> {
    let sql = `SELECT * FROM proposals WHERE status = $status ORDER BY rowid ASC`;
    const params: Params = { $status: status };
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as ProposalRow[];
    return rows.map(proposalFromRow);
  }

  async transition(
    proposalId: string,
    from: ProposalStatus[],
    to: ProposalStatus,
    patch?: ProposalPatch,
  ): Promise<ProposalRecord> {
    return this.conn.whenFree(() => {
      const row = this.selectProposalRow(proposalId);
      if (!row)
        throw new RecordNotFoundError(`Proposal not found: ${proposalId}`);
      const current = row.status as ProposalStatus;
      if (!from.includes(current)) {
        throw new InvalidProposalTransitionError(
          `Proposal ${proposalId} is ${current}, expected one of [${from.join(", ")}].`,
          { proposalId, current, from, to },
        );
      }
      assertProposalTransition(current, to);
      const result = this.conn.run(
        `UPDATE proposals SET
           status = $status,
           decision = COALESCE($decision, decision),
           decided_at = COALESCE($decidedAt, decided_at),
           claimed_at = COALESCE($claimedAt, claimed_at),
           applied_at = COALESCE($appliedAt, applied_at),
           operation_id = COALESCE($operationId, operation_id),
           reason = COALESCE($reason, reason)
         WHERE id = $id AND status = $current`,
        {
          $status: to,
          $decision:
            patch?.decision !== undefined ? toJson(patch.decision) : null,
          $decidedAt: patch?.decidedAt ?? null,
          $claimedAt: patch?.claimedAt ?? null,
          $appliedAt: patch?.appliedAt ?? null,
          $operationId: patch?.operationId ?? null,
          $reason: patch?.reason ?? null,
          $id: proposalId,
          $current: current,
        },
      );
      if (result.changes === 0) {
        throw new InvalidProposalTransitionError(
          `Proposal ${proposalId} changed concurrently; expected one of [${from.join(", ")}].`,
          { proposalId, from, to },
        );
      }
      return proposalFromRow(this.selectProposalRow(proposalId)!);
    }, this.txOwner);
  }

  async recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome> {
    return this.conn.whenFree(() => {
      const existing = this.conn.get(
        `SELECT * FROM proposal_outcomes WHERE operation_id = $id`,
        { $id: operationId },
      ) as ProposalOutcomeRow | undefined;
      // Idempotent on operationId: the first outcome is the one that
      // happened; a later call must not overwrite the evidence.
      if (existing) return outcomeFromRow(existing);
      this.conn.run(
        `INSERT INTO proposal_outcomes (operation_id, status, applied_ops, failed_ops, result_json, revision)
         VALUES ($id, $status, $appliedOps, $failedOps, $resultJson, $revision)`,
        {
          $id: operationId,
          $status: outcome.status,
          $appliedOps: outcome.appliedOps,
          $failedOps: toJson(outcome.failedOps),
          $resultJson: outcome.resultJson ?? null,
          $revision: outcome.revision ?? null,
        },
      );
      return outcome;
    }, this.txOwner);
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    const row = this.conn.get(
      `SELECT * FROM proposal_outcomes WHERE operation_id = $id`,
      { $id: operationId },
    ) as ProposalOutcomeRow | undefined;
    return row ? outcomeFromRow(row) : null;
  }

  async invalidatePendingForRevision(
    scopeKey: string,
    newRevision: string,
  ): Promise<number> {
    return this.conn.whenFree(() => {
      const rows = this.conn.all(
        `SELECT * FROM proposals WHERE scope_key = $scopeKey AND status = 'pending'
           AND (revision_at_create IS NULL OR revision_at_create != $newRevision)`,
        { $scopeKey: scopeKey, $newRevision: newRevision },
      ) as ProposalRow[];
      const at = this.clock.nowIso();
      for (const row of rows) {
        assertProposalTransition("pending", "invalidated");
        this.conn.run(
          `UPDATE proposals SET status = 'invalidated', reason = 'revision_conflict', decided_at = $at
           WHERE id = $id AND status = 'pending'`,
          { $at: at, $id: row.id },
        );
      }
      return rows.length;
    }, this.txOwner);
  }

  /**
   * Delete a chat's proposals and the outcomes they claimed, in ONE
   * transaction.
   *
   * BY `chat_id`, never by `scope_key` — see the port: two chats routinely
   * propose writes into one shared scope, and deleting the scope would take a
   * bystander's staged writes with it.
   *
   * Outcomes first: they are keyed by `operation_id`, which only the proposal
   * row still names, so deleting the proposals first would leave rows nothing
   * can ever identify again.
   */
  async deleteByChat(chatId: string): Promise<number> {
    return this.conn.whenFree(() => {
      const params: Params = { $chatId: chatId };
      this.conn.run(
        `DELETE FROM proposal_outcomes WHERE operation_id IN (
           SELECT operation_id FROM proposals
            WHERE chat_id = $chatId AND operation_id IS NOT NULL)`,
        params,
      );
      return this.conn.run(
        `DELETE FROM proposals WHERE chat_id = $chatId`,
        params,
      ).changes;
    }, this.txOwner);
  }

  private selectProposalRow(proposalId: string): ProposalRow | null {
    return (
      (this.conn.get(`SELECT * FROM proposals WHERE id = $id`, {
        $id: proposalId,
      }) as ProposalRow | undefined) ?? null
    );
  }
}
