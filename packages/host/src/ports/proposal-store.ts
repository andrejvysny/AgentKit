/**
 * Lifecycle of a staged write.
 *
 * `pending → approved → applying → applied|failed` is the whole happy path;
 * `pending` also exits to `rejected` (a human said no) and `invalidated` (the
 * world moved on, so what was reviewed is no longer what would be written).
 *
 * `applying` is a persisted state, not a variable in a function: a process that
 * dies mid-apply leaves the record there, and recovery can then ask the applier
 * whether the side effects landed instead of guessing.
 */
export type ProposalStatus =
  | "pending"
  | "approved"
  | "applying"
  | "applied"
  | "failed"
  | "rejected"
  | "invalidated";

/**
 * How bad it would be if this write were wrong. Ordered low → destructive; the
 * write policy grants allowances by rank, and rank N covers everything ≤ N.
 */
export type RiskLevel = "low" | "medium" | "high" | "destructive";

/** Who decided, and on what authority. Persisted so an audit can tell them apart. */
export interface ProposalDecision {
  /**
   * `"policy"` decisions are machine approvals under a standing allowance and
   * MUST carry `policyId`. Recording one as a user decision would let an
   * auto-applied write masquerade as human-reviewed.
   */
  actor: "user" | "policy";
  decidedBy?: string;
  policyId?: string;
  reason?: string;
  decidedAt: string;
}

export interface ProposalRecord {
  id: string;
  chatId: string;
  runId?: string;
  /** What this write touches; the serialization + idempotency namespace. */
  scopeKey: string;
  /** Model-supplied idempotency key. Unique per scope when present. */
  actionId?: string;
  toolName: string;
  /** Host-defined proposal kind (the write's shape), e.g. "document.edits". */
  kind: string;
  risk: RiskLevel;
  status: ProposalStatus;
  /** The full staged payload, as the tool built it. */
  envelope: Record<string, unknown>;
  operations: unknown[];
  warnings: string[];
  /** The builder dropped operations to fit a cap; never auto-apply a truncated write. */
  truncated: boolean;
  /** Scope revision the proposal was built against; the staleness check. */
  revisionAtCreate?: string;
  /**
   * The apply attempt that claimed this proposal (written at the claim, before
   * any side effect). Crash recovery reads it to ask the applier what happened;
   * without it, an interrupted `applying` record is unresolvable.
   */
  operationId?: string;
  decision?: ProposalDecision;
  /**
   * Why this proposal ended where it did (`revision_conflict`, `interrupted`, an
   * applier's error). Written by the transition that terminates it, and read
   * back by the UI and by the model correcting its own work — "failed" without a
   * reason is not actionable by either.
   */
  reason?: string;
  createdAt: string;
  decidedAt?: string;
  appliedAt?: string;
}

/**
 * What one apply attempt did, keyed by operation id and persisted verbatim.
 *
 * `partial` is a real outcome, not a rounding of success or failure: some
 * operations landed and some did not, and both halves matter to the model that
 * has to correct the rest. The state machine still ends at `applied` (the write
 * happened); the partiality lives here.
 */
export interface ApplyOutcome {
  status: "applied" | "partial" | "failed";
  appliedOps: number;
  failedOps: { opIndex: number; error: string }[];
  /** Host payload describing the result (ids created, etc.). */
  resultJson?: string;
  /** Scope revision AFTER the apply. */
  revision?: string;
}

export interface CreateProposalInput {
  id: string;
  chatId: string;
  runId?: string;
  scopeKey: string;
  actionId?: string;
  toolName: string;
  kind: string;
  risk: RiskLevel;
  envelope: Record<string, unknown>;
  operations: unknown[];
  warnings: string[];
  truncated: boolean;
  revisionAtCreate?: string;
  createdAt: string;
}

/** Fields a transition may write alongside the status change, atomically. */
export interface ProposalPatch {
  decision?: ProposalDecision;
  decidedAt?: string;
  appliedAt?: string;
  operationId?: string;
  /** Why it failed / was invalidated; surfaced to the model and the UI. */
  reason?: string;
}

export interface ListProposalsOptions {
  status?: ProposalStatus;
  limit?: number;
}

export interface ProposalStore {
  /**
   * MUST enforce UNIQUE `(scopeKey, actionId)` when `actionId` is set and throw
   * {@link DuplicateActionIdError} on collision — that pair IS the idempotency
   * guarantee a write tool advertises to the model.
   */
  create(input: CreateProposalInput): Promise<ProposalRecord>;
  get(proposalId: string): Promise<ProposalRecord | null>;
  getByActionId(
    scopeKey: string,
    actionId: string,
  ): Promise<ProposalRecord | null>;
  listByChat(
    chatId: string,
    opts?: ListProposalsOptions,
  ): Promise<ProposalRecord[]>;
  /**
   * Cross-chat status query. Crash recovery needs "every proposal stuck in
   * `applying`", which no per-chat listing can answer.
   */
  listByStatus(
    status: ProposalStatus,
    opts?: { limit?: number },
  ): Promise<ProposalRecord[]>;

  /**
   * Compare-and-set the proposal's status. MUST reject with
   * {@link InvalidProposalTransitionError} when the current status is not in
   * `from`, or when the change is not in `PROPOSAL_TRANSITIONS`.
   */
  transition(
    proposalId: string,
    from: ProposalStatus[],
    to: ProposalStatus,
    patch?: ProposalPatch,
  ): Promise<ProposalRecord>;

  /**
   * Record what an apply attempt did. Idempotent on `operationId`: a second call
   * with the same id MUST keep the first outcome and return it, because the id
   * identifies work that happened once and a "corrected" outcome would erase the
   * evidence a replay depends on.
   */
  recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome>;
  getOutcome(operationId: string): Promise<ApplyOutcome | null>;

  /**
   * Invalidate every still-`pending` proposal in a scope because the scope moved
   * to `newRevision`. Returns how many were invalidated.
   */
  invalidatePendingForRevision(
    scopeKey: string,
    newRevision: string,
  ): Promise<number>;
}
