import {
  AgentKitHostError,
  DuplicateActionIdError,
  RecordNotFoundError,
  RevisionConflictError,
} from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { ProposalApplier } from "../ports/proposal-applier.js";
import type {
  ApplyOutcome,
  ProposalDecision,
  ProposalRecord,
  RiskLevel,
} from "../ports/proposal-store.js";
import type { Clock, IdGenerator, Logger } from "../ports/system.js";
import type { WritePolicy } from "../ports/write-policy.js";

export interface ProposalServiceDeps {
  store: AssistantStore;
  applier: ProposalApplier;
  /**
   * The auto-apply policy in force. The service does not consult it — the write
   * tool does, before asking for a policy approval — but it is carried here so
   * one wiring point hands the whole proposal pipeline the same policy object.
   */
  policy: WritePolicy;
  clock: Clock;
  ids: IdGenerator;
  logger?: Logger;
}

export interface StageProposalInput {
  /** Pre-minted id, when the caller needs it before staging (e.g. an envelope). */
  id?: string;
  chatId: string;
  runId?: string;
  scopeKey: string;
  actionId?: string;
  toolName: string;
  kind: string;
  risk: RiskLevel;
  envelope?: Record<string, unknown>;
  operations: unknown[];
  warnings?: string[];
  truncated?: boolean;
  revisionAtCreate?: string;
}

export interface ApproveProposalInput {
  proposalId: string;
  actor: ProposalDecision["actor"];
  decidedBy?: string;
  /** REQUIRED for `actor: "policy"`, forbidden for `actor: "user"`. */
  policyId?: string;
  reason?: string;
}

export interface RejectProposalInput {
  proposalId: string;
  decidedBy?: string;
  reason?: string;
}

export interface ApplyProposalRequest {
  proposalId: string;
  /** Idempotency key for this apply attempt; replaying it replays the outcome. */
  operationId: string;
  signal?: AbortSignal;
}

export interface InvalidateForRevisionInput {
  scopeKey: string;
  newRevision: string;
}

export interface ReconcileReport {
  /** Proposals found stuck in `applying`. */
  reconciled: number;
  /** Of those, ones the applier could prove had landed. */
  applied: number;
  /** Of those, ones finalized as failed (including "interrupted"). */
  failed: number;
}

/** Reason recorded when a proposal is refused because its scope moved on. */
export const REVISION_CONFLICT_REASON = "revision_conflict";
/** Reason recorded when a crash left an apply with no provable outcome. */
export const INTERRUPTED_REASON = "interrupted";

/**
 * The lifecycle of a staged write: stage → decide → apply, with the crash and
 * replay paths that make "apply" safe to say twice.
 *
 * The design rests on three separations:
 *
 * 1. **Deciding is not applying.** `approve` records who said yes and on what
 *    authority; `apply` performs the work. A single "approveAndApply" would make
 *    it impossible to tell an auto-applied write from a reviewed one after the
 *    fact, and that distinction is the whole point of having a policy.
 * 2. **The outcome is keyed by the operation, not the proposal.** Whoever calls
 *    `apply` twice with the same `operationId` gets the first outcome back
 *    verbatim and the applier is never re-invoked. This is what makes the queue
 *    able to redeliver a message without duplicating a write.
 * 3. **`applying` is durable.** A process that dies mid-apply leaves a record
 *    saying so, and {@link ProposalService.reconcileInterrupted} resolves it by
 *    asking the applier what actually happened rather than guessing.
 */
export class ProposalService {
  constructor(private readonly deps: ProposalServiceDeps) {}

  /** The policy this pipeline was wired with (see {@link ProposalServiceDeps}). */
  get policy(): WritePolicy {
    return this.deps.policy;
  }

  /**
   * Persist a proposal in `pending`. Always called before any auto-apply
   * decision: a write that applied without a record behind it is a write nobody
   * can audit, undo, or reconcile after a crash.
   */
  async stage(input: StageProposalInput): Promise<ProposalRecord> {
    const id = input.id ?? this.deps.ids.proposalId();
    try {
      return await this.deps.store.proposals.create({
        id,
        chatId: input.chatId,
        ...(input.runId === undefined ? {} : { runId: input.runId }),
        scopeKey: input.scopeKey,
        ...(input.actionId === undefined ? {} : { actionId: input.actionId }),
        toolName: input.toolName,
        kind: input.kind,
        risk: input.risk,
        envelope: input.envelope ?? {},
        operations: input.operations,
        warnings: input.warnings ?? [],
        truncated: input.truncated ?? false,
        ...(input.revisionAtCreate === undefined
          ? {}
          : { revisionAtCreate: input.revisionAtCreate }),
        createdAt: this.deps.clock.nowIso(),
      });
    } catch (err) {
      // Re-throw the store's uniqueness violation with the context a caller
      // needs to react (the write tool turns it into a "you already did this"
      // answer for the model). The store only knows the constraint fired.
      if (err instanceof DuplicateActionIdError) {
        throw new DuplicateActionIdError(
          `A proposal with action_id "${input.actionId}" already exists in scope "${input.scopeKey}".`,
          {
            scopeKey: input.scopeKey,
            actionId: input.actionId,
            toolName: input.toolName,
          },
        );
      }
      throw err;
    }
  }

  /**
   * Record a decision to apply. `pending → approved`.
   *
   * A policy approval MUST carry its `policyId` and a user approval MUST NOT:
   * the audit trail has to say whether a human looked at this, and a decision
   * that could be read either way is worse than no decision record at all.
   */
  async approve(input: ApproveProposalInput): Promise<ProposalRecord> {
    if (input.actor === "policy" && !input.policyId) {
      throw new AgentKitHostError(
        "invalid_decision",
        "A policy approval must carry the policyId that authorised it.",
        { proposalId: input.proposalId },
      );
    }
    if (input.actor === "user" && input.policyId !== undefined) {
      throw new AgentKitHostError(
        "invalid_decision",
        "A user approval must not carry a policyId — it would read as a machine decision.",
        { proposalId: input.proposalId },
      );
    }
    const decidedAt = this.deps.clock.nowIso();
    const decision: ProposalDecision = {
      actor: input.actor,
      ...(input.decidedBy === undefined ? {} : { decidedBy: input.decidedBy }),
      ...(input.policyId === undefined ? {} : { policyId: input.policyId }),
      ...(input.reason === undefined ? {} : { reason: input.reason }),
      decidedAt,
    };
    return this.deps.store.proposals.transition(
      input.proposalId,
      ["pending"],
      "approved",
      { decision, decidedAt },
    );
  }

  /** Record a refusal. `pending → rejected`. Always a human decision. */
  async reject(input: RejectProposalInput): Promise<ProposalRecord> {
    const decidedAt = this.deps.clock.nowIso();
    const decision: ProposalDecision = {
      actor: "user",
      ...(input.decidedBy === undefined ? {} : { decidedBy: input.decidedBy }),
      ...(input.reason === undefined ? {} : { reason: input.reason }),
      decidedAt,
    };
    return this.deps.store.proposals.transition(
      input.proposalId,
      ["pending"],
      "rejected",
      { decision, decidedAt },
    );
  }

  /**
   * Apply an approved proposal, exactly once per `operationId`.
   *
   * Order of operations, and why each step precedes the next:
   *
   * 1. **Replay check.** An outcome already recorded for this operation id is
   *    returned verbatim and the applier is not called. A proposal still sitting
   *    in `applying` (the crash-between-apply-and-finalize case) is finalized to
   *    match, so the record and the outcome cannot disagree.
   * 2. **Staleness check, BEFORE claiming.** If the scope moved since the
   *    proposal was built, what the reviewer saw is not what would be written.
   *    A still-`pending` proposal is invalidated; one already approved is claimed
   *    and failed with `revision_conflict`, so the refused attempt is visible in
   *    its history.
   * 3. **Claim** `approved → applying`, writing the operation id onto the record
   *    — before any side effect, so recovery can always find the operation to
   *    ask the applier about.
   * 4. **Apply**, then **record the outcome**, then finalize. Recording before
   *    the status change means a crash in between leaves an `applying` record
   *    whose outcome is discoverable, which reconcile resolves without guessing.
   *
   * A `partial` outcome finalizes to `applied`: the write happened, and the
   * partiality lives on the outcome where the model and the UI read it. Collapsing
   * it to `failed` would invite a retry of work that already half-landed.
   */
  async apply(input: ApplyProposalRequest): Promise<ApplyOutcome> {
    const proposals = this.deps.store.proposals;

    const replayed = await this.findRecordedOutcome(input.operationId);
    if (replayed) {
      const current = await proposals.get(input.proposalId);
      if (current?.status === "applying") {
        await this.finalizeFromOutcome(input.proposalId, replayed);
      }
      this.deps.logger?.debug("proposal apply replayed", {
        proposalId: input.proposalId,
        operationId: input.operationId,
        status: replayed.status,
      });
      return replayed;
    }

    const proposal = await proposals.get(input.proposalId);
    if (!proposal) {
      throw new RecordNotFoundError(`Proposal not found: ${input.proposalId}`, {
        proposalId: input.proposalId,
      });
    }

    await this.guardRevision(proposal, input.operationId);

    const claimed = await proposals.transition(
      input.proposalId,
      ["approved"],
      "applying",
      { operationId: input.operationId },
    );

    let outcome: ApplyOutcome;
    try {
      outcome = await this.deps.applier.apply({
        proposal: claimed,
        operationId: input.operationId,
        ...(input.signal === undefined ? {} : { signal: input.signal }),
      });
    } catch (err) {
      // The applier threw, so we know nothing about what landed. Record a failed
      // outcome anyway: leaving `applying` without one would make this proposal
      // indistinguishable from a crash, and send recovery asking the applier a
      // question it has already answered by throwing.
      const message = err instanceof Error ? err.message : String(err);
      const synthesized: ApplyOutcome = {
        status: "failed",
        appliedOps: 0,
        // The applier reported no per-operation detail; attribute the failure to
        // the first operation rather than inventing indices for all of them.
        failedOps:
          claimed.operations.length > 0 ? [{ opIndex: 0, error: message }] : [],
      };
      const recorded = await proposals.recordOutcome(
        input.operationId,
        synthesized,
      );
      await this.finalizeFromOutcome(input.proposalId, recorded);
      throw err;
    }

    const recorded = await proposals.recordOutcome(input.operationId, outcome);
    await this.finalizeFromOutcome(input.proposalId, recorded);
    return recorded;
  }

  /**
   * Resolve every proposal left in `applying` by a crash.
   *
   * The applier — not this service — is the authority on whether the side effects
   * landed, so each stuck record is settled by looking its operation id up. When
   * nothing is recorded, the attempt is finalized as failed with reason
   * `interrupted`: "we cannot prove it landed" must be treated exactly like "it
   * did not", because a retry of a write that silently succeeded is the one
   * outcome nobody can undo.
   */
  async reconcileInterrupted(): Promise<ReconcileReport> {
    const proposals = this.deps.store.proposals;
    const stuck = await proposals.listByStatus("applying");
    const report: ReconcileReport = {
      reconciled: stuck.length,
      applied: 0,
      failed: 0,
    };
    for (const proposal of stuck) {
      const outcome = proposal.operationId
        ? await this.findRecordedOutcome(proposal.operationId)
        : null;
      if (outcome) {
        await this.finalizeFromOutcome(proposal.id, outcome);
        if (outcome.status === "failed") report.failed++;
        else report.applied++;
        continue;
      }
      if (proposal.operationId) {
        await proposals.recordOutcome(proposal.operationId, {
          status: "failed",
          appliedOps: 0,
          failedOps: [],
        });
      }
      await proposals.transition(proposal.id, ["applying"], "failed", {
        reason: INTERRUPTED_REASON,
      });
      report.failed++;
      this.deps.logger?.warn("proposal apply interrupted", {
        proposalId: proposal.id,
        operationId: proposal.operationId,
      });
    }
    return report;
  }

  /**
   * Invalidate every pending proposal in a scope that moved to `newRevision`.
   * Called by the host when its state changes underneath staged writes; returns
   * how many were invalidated.
   */
  async invalidateForRevision(
    input: InvalidateForRevisionInput,
  ): Promise<number> {
    return this.deps.store.proposals.invalidatePendingForRevision(
      input.scopeKey,
      input.newRevision,
    );
  }

  /**
   * The recorded outcome for an operation id, from our own store first and the
   * applier second.
   *
   * The applier fallback covers the crash window between "the write landed" and
   * "we wrote that down": the applier remembers, we do not, and the difference
   * between asking it and not asking it is a duplicated write. Anything it
   * reports is persisted on the way through, so the store is authoritative from
   * then on.
   */
  private async findRecordedOutcome(
    operationId: string,
  ): Promise<ApplyOutcome | null> {
    const stored = await this.deps.store.proposals.getOutcome(operationId);
    if (stored) return stored;
    const external = await this.deps.applier.getOutcome(operationId);
    if (!external) return null;
    return this.deps.store.proposals.recordOutcome(operationId, external);
  }

  /** `applying → applied | failed`, per the outcome. */
  private async finalizeFromOutcome(
    proposalId: string,
    outcome: ApplyOutcome,
  ): Promise<ProposalRecord> {
    if (outcome.status === "failed") {
      return this.deps.store.proposals.transition(
        proposalId,
        ["applying"],
        "failed",
        { reason: describeFailure(outcome) },
      );
    }
    return this.deps.store.proposals.transition(
      proposalId,
      ["applying"],
      "applied",
      { appliedAt: this.deps.clock.nowIso() },
    );
  }

  /**
   * Refuse to apply a proposal whose scope moved on, leaving the record in a
   * state that says why. See {@link ProposalService.apply} step 2.
   */
  private async guardRevision(
    proposal: ProposalRecord,
    operationId: string,
  ): Promise<void> {
    const currentRevision = this.deps.applier.currentRevision;
    if (!currentRevision || proposal.revisionAtCreate === undefined) return;
    const now = await currentRevision.call(
      this.deps.applier,
      proposal.scopeKey,
    );
    if (now === null || now === proposal.revisionAtCreate) return;

    const proposals = this.deps.store.proposals;
    const details = {
      proposalId: proposal.id,
      scopeKey: proposal.scopeKey,
      revisionAtCreate: proposal.revisionAtCreate,
      currentRevision: now,
    };
    if (proposal.status === "pending") {
      await proposals.transition(proposal.id, ["pending"], "invalidated", {
        reason: REVISION_CONFLICT_REASON,
        decidedAt: this.deps.clock.nowIso(),
      });
    } else {
      // Already approved: the state machine has no approved → invalidated edge
      // (an approval is a fact, not something to erase), so the refusal is
      // recorded as a claimed-and-failed apply attempt.
      await proposals.transition(proposal.id, ["approved"], "applying", {
        operationId,
      });
      await proposals.recordOutcome(operationId, {
        status: "failed",
        appliedOps: 0,
        failedOps: [],
      });
      await proposals.transition(proposal.id, ["applying"], "failed", {
        reason: REVISION_CONFLICT_REASON,
      });
    }
    throw new RevisionConflictError(
      `Scope "${proposal.scopeKey}" changed since the proposal was built (was ${proposal.revisionAtCreate}, now ${now}). Rebuild the write.`,
      details,
    );
  }
}

/** One line describing a failed outcome, for the record's `reason`. */
function describeFailure(outcome: ApplyOutcome): string {
  if (outcome.failedOps.length === 0) return "apply_failed";
  return outcome.failedOps
    .map((op) => `op ${op.opIndex}: ${op.error}`)
    .join("; ");
}
