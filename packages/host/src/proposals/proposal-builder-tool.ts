import type { AiToolDefinition, AiToolResult } from "@agentkit/contracts";
import type { AiTool, AiToolExecutionContext } from "@agentkit/core";
import type { AssistantStore } from "../ports/assistant-store.js";
import type {
  ApplyOutcome,
  ProposalRecord,
  ProposalStatus,
  RiskLevel,
} from "../ports/proposal-store.js";
import type { IdGenerator, Logger } from "../ports/system.js";
import type { WritePolicy } from "../ports/write-policy.js";
import { normalizeActionId } from "./action-id.js";
import type { ProposalService } from "./proposal-service.js";

/**
 * What the model is told about a write, and nothing more.
 *
 * Four statuses, a count, and the things that did not happen with a reason each.
 * A model correcting its own work needs to know what landed and what to redo;
 * handing it the full apply payload instead buries that in noise and burns
 * context on data it cannot act on.
 */
export interface WriteToolModelData {
  status: "ok" | "partial" | "pending" | "already_applied";
  appliedCount: number;
  skipped: { id: string; reason: string }[];
}

/** UI-facing projection of the proposal a write tool staged. */
export interface ProposalToolData {
  proposalId: string;
  status: ProposalStatus;
  kind: string;
  risk: RiskLevel;
  operationCount: number;
  envelope: Record<string, unknown>;
}

/** What a host's builder produces from validated tool input. */
export interface ProposalBuildResult {
  kind: string;
  risk: RiskLevel;
  operations: unknown[];
  /** Per-item problems found while building (unresolvable targets, skips). */
  warnings: string[];
  /** Operations were dropped to fit a cap — blocks auto-apply. */
  truncated: boolean;
  envelope?: Record<string, unknown>;
  summary?: string;
}

export interface ProposalBuilderToolOptions<TInput> {
  /** Must declare `effect: "write"`; a read tool has nothing to propose. */
  definition: AiToolDefinition;
  service: ProposalService;
  store: AssistantStore;
  policy: WritePolicy;
  ids: IdGenerator;
  /** The scope this call writes to — the idempotency + serialization namespace. */
  scopeKeyOf(ctx: AiToolExecutionContext, input: TInput): string;
  /** Scope revision to stamp on the proposal, enabling the staleness check. */
  currentRevision?(scopeKey: string): Promise<string | null>;
  /** Turn validated input into operations. Host domain logic lives here only. */
  build(
    ctx: AiToolExecutionContext,
    input: TInput,
  ): Promise<ProposalBuildResult>;
  /** Recorded on policy approvals; identifies what authorised the auto-apply. */
  policyId?: string;
  logger?: Logger;
}

const DEFAULT_POLICY_ID = "session-write-policy";

/**
 * Build a write tool that stages a proposal and — only when the policy says so —
 * applies it in the same call.
 *
 * The invariants this wrapper exists to enforce, which every host write tool
 * would otherwise re-implement (and eventually get wrong):
 *
 * - **Stage first, always.** The proposal is persisted before any apply is
 *   considered, so a write that lands is always backed by a record, and one that
 *   waits is visible in the UI instead of lost in a tool's local variables.
 * - **Dedup on `(scope, action_id)`.** Re-issuing a key that already applied is
 *   answered from the record, never re-executed; one still in flight or already
 *   failed is refused with an explanation the model can act on.
 * - **The gate is conservative.** No operations, a truncated build, a destructive
 *   write carrying warnings, or no standing allowance ⇒ it waits.
 * - **An apply failure is a result, not an exception.** The proposal is left
 *   `failed` and the tool returns `ok: false`, because a throw here would be
 *   reported to the model as "the tool crashed" — losing the fact that the write
 *   was attempted and the reason it did not land.
 */
export function createProposalBuilderTool<TInput>(
  options: ProposalBuilderToolOptions<TInput>,
): AiTool<TInput, ProposalToolData | null> {
  if (options.definition.effect !== "write") {
    throw new Error(
      `createProposalBuilderTool requires effect:"write"; "${options.definition.name}" declares "${options.definition.effect}".`,
    );
  }
  const toolName = options.definition.name;
  const policyId = options.policyId ?? DEFAULT_POLICY_ID;

  return {
    definition: options.definition,
    async execute(
      ctx: AiToolExecutionContext,
      input: TInput,
    ): Promise<AiToolResult<ProposalToolData | null>> {
      const warnings: string[] = [];
      const chatId = ctx.chatId;
      if (chatId === undefined) {
        // A write is owned by a conversation; without one there is nothing to
        // stage it against. A wiring bug, reported rather than thrown so the run
        // survives it.
        return {
          ok: false,
          summary: `${toolName} requires a chat context.`,
          data: null,
          modelData: {
            status: "partial",
            appliedCount: 0,
            skipped: [{ id: "context", reason: "missing chatId" }],
          } satisfies WriteToolModelData,
          sources: [],
          warnings: [
            "Tool executed without a chat context; nothing was staged.",
          ],
          truncated: false,
          limits: ctx.limits,
        };
      }

      const scopeKey = options.scopeKeyOf(ctx, input);
      const actionId = normalizeActionId(
        (input as { action_id?: unknown } | null)?.action_id,
        warnings,
      );

      // 1. Idempotency: has this exact intent already been written?
      if (actionId !== undefined) {
        const prior = await options.store.proposals.getByActionId(
          scopeKey,
          actionId,
        );
        const dedup = prior
          ? await dedupResult(prior, actionId, ctx, options, warnings)
          : null;
        if (dedup) return dedup;
      }

      // 2. Build, then stage — in that order, and unconditionally.
      const built = await options.build(ctx, input);
      const revisionAtCreate = options.currentRevision
        ? ((await options.currentRevision(scopeKey)) ?? undefined)
        : undefined;
      const proposal = await options.service.stage({
        chatId,
        ...(ctx.runId === undefined ? {} : { runId: ctx.runId }),
        scopeKey,
        ...(actionId === undefined ? {} : { actionId }),
        toolName,
        kind: built.kind,
        risk: built.risk,
        envelope: built.envelope ?? {},
        operations: built.operations,
        warnings: built.warnings,
        truncated: built.truncated,
        ...(revisionAtCreate === undefined ? {} : { revisionAtCreate }),
      });

      const allWarnings = [...warnings, ...built.warnings];
      const buildSkipped = built.warnings.map((reason) => ({
        id: "build",
        reason,
      }));

      // 3. The auto-apply gate. Every clause is a reason to stop and ask:
      //    nothing to do; a build that dropped operations (applying half a write
      //    silently commits a design nobody described); a destructive write that
      //    already knows something went wrong; or no standing allowance.
      const autoApply =
        built.operations.length > 0 &&
        !built.truncated &&
        (built.risk !== "destructive" || allWarnings.length === 0) &&
        options.policy.isAutoApplyAllowed({
          chatId,
          toolName,
          proposalKind: built.kind,
          risk: built.risk,
        });

      if (!autoApply) {
        return {
          ok: true,
          status: buildSkipped.length > 0 ? "partial" : "ok",
          summary:
            built.summary ??
            `Staged ${built.operations.length} operation(s) awaiting confirmation.`,
          data: projectProposal(proposal),
          modelData: {
            status: buildSkipped.length > 0 ? "partial" : "pending",
            appliedCount: 0,
            skipped: buildSkipped,
          } satisfies WriteToolModelData,
          sources: [],
          warnings: allWarnings,
          truncated: built.truncated,
          limits: ctx.limits,
        };
      }

      // 4. Policy approval + apply. Never allowed to throw out of the tool.
      try {
        await options.service.approve({
          proposalId: proposal.id,
          actor: "policy",
          policyId,
          reason: `auto-apply allowed for ${toolName}/${built.kind}`,
        });
        const outcome = await options.service.apply({
          proposalId: proposal.id,
          operationId: options.ids.operationId(),
          ...(ctx.signal === undefined ? {} : { signal: ctx.signal }),
        });
        return applyResult(proposal, outcome, buildSkipped, allWarnings, ctx);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        options.logger?.warn("write tool auto-apply failed", {
          toolName,
          proposalId: proposal.id,
          error: message,
        });
        return {
          ok: false,
          status: "partial",
          summary: `Auto-apply failed: ${message}`,
          data: projectProposal({ ...proposal, status: "failed" }),
          modelData: {
            status: "partial",
            appliedCount: 0,
            skipped: [...buildSkipped, { id: "apply", reason: message }],
          } satisfies WriteToolModelData,
          sources: [],
          warnings: [...allWarnings, `Auto-apply failed: ${message}`],
          truncated: built.truncated,
          limits: ctx.limits,
        };
      }
    },
  };
}

/** The UI-facing half of a tool result. */
function projectProposal(proposal: ProposalRecord): ProposalToolData {
  return {
    proposalId: proposal.id,
    status: proposal.status,
    kind: proposal.kind,
    risk: proposal.risk,
    operationCount: proposal.operations.length,
    envelope: proposal.envelope,
  };
}

/**
 * Decide what a repeated `action_id` means.
 *
 * - applied → answer from the record (`ok: true`); the work exists, so reporting
 *   failure would push the model into redoing it.
 * - pending / approved / applying → refuse: an identical write is in flight, and
 *   a second dispatch is exactly the duplicate the key exists to prevent.
 * - failed → refuse, and say to use a NEW key: the prior attempt may have partly
 *   landed, so a blind repeat could double-write whatever did.
 * - rejected / invalidated → nothing is in flight and nothing landed; a fresh
 *   attempt is legitimate (returns null to continue).
 */
async function dedupResult<TInput>(
  prior: ProposalRecord,
  actionId: string,
  ctx: AiToolExecutionContext,
  options: ProposalBuilderToolOptions<TInput>,
  warnings: string[],
): Promise<AiToolResult<ProposalToolData | null> | null> {
  if (prior.status === "applied") {
    const outcome = prior.operationId
      ? await options.store.proposals.getOutcome(prior.operationId)
      : null;
    return {
      ok: true,
      status: "ok",
      summary: `already_applied: ${actionId}`,
      data: projectProposal(prior),
      modelData: {
        status: "already_applied",
        appliedCount: outcome?.appliedOps ?? 0,
        skipped: [],
      } satisfies WriteToolModelData,
      sources: [],
      warnings,
      truncated: false,
      limits: ctx.limits,
    };
  }

  if (
    prior.status === "pending" ||
    prior.status === "approved" ||
    prior.status === "applying"
  ) {
    const reason = `action_id "${actionId}" is already in-flight (${prior.status}); do not re-issue it. Wait for it to settle instead of repeating the write.`;
    return blockedResult(prior, actionId, reason, ctx, warnings);
  }

  if (prior.status === "failed") {
    const reason = `action_id "${actionId}" was already attempted and failed; do not re-issue it. Inspect the current state and use a new action_id for whatever still needs doing.`;
    return blockedResult(prior, actionId, reason, ctx, warnings);
  }

  // rejected / invalidated: nothing is in flight, nothing landed.
  return null;
}

/**
 * A refused duplicate. `ok: false` on purpose — the run loop must not count a
 * blocked call as progress — but `status: "partial"` rather than an error, since
 * nothing went wrong: the guard worked.
 */
function blockedResult(
  prior: ProposalRecord,
  actionId: string,
  reason: string,
  ctx: AiToolExecutionContext,
  warnings: string[],
): AiToolResult<ProposalToolData | null> {
  return {
    ok: false,
    status: "partial",
    summary: `duplicate_blocked: ${actionId}`,
    data: projectProposal(prior),
    modelData: {
      status: "already_applied",
      appliedCount: 0,
      skipped: [{ id: actionId, reason }],
    } satisfies WriteToolModelData,
    sources: [],
    warnings: [...warnings, reason],
    truncated: false,
    limits: ctx.limits,
  };
}

/**
 * Project an apply outcome into a tool result.
 *
 * A partial apply reports `ok: false` / `status: "partial"`: some of what the
 * model asked for did not happen, and a clean success line would leave it
 * believing otherwise — the failure mode where an agent narrates a finished job
 * over a half-finished one.
 */
function applyResult(
  proposal: ProposalRecord,
  outcome: ApplyOutcome,
  buildSkipped: { id: string; reason: string }[],
  warnings: string[],
  ctx: AiToolExecutionContext,
): AiToolResult<ProposalToolData | null> {
  const failedSkips = outcome.failedOps.map((op) => ({
    id: `op:${op.opIndex}`,
    reason: op.error,
  }));
  const skipped = [...buildSkipped, ...failedSkips];
  const landed = outcome.status !== "failed";
  const status: ProposalStatus = landed ? "applied" : "failed";

  if (outcome.status === "applied") {
    return {
      ok: true,
      status: "ok",
      summary: `Applied ${outcome.appliedOps} operation(s).`,
      data: projectProposal({ ...proposal, status }),
      modelData: {
        status: "ok",
        appliedCount: outcome.appliedOps,
        skipped,
      } satisfies WriteToolModelData,
      sources: [],
      warnings,
      truncated: false,
      limits: ctx.limits,
    };
  }

  const summary =
    outcome.status === "partial"
      ? `Applied ${outcome.appliedOps} operation(s); ${outcome.failedOps.length} failed.`
      : `Apply failed after ${outcome.appliedOps} operation(s).`;
  return {
    ok: false,
    status: "partial",
    summary,
    data: projectProposal({ ...proposal, status }),
    modelData: {
      status: "partial",
      appliedCount: outcome.appliedOps,
      skipped,
    } satisfies WriteToolModelData,
    sources: [],
    warnings,
    truncated: false,
    limits: ctx.limits,
  };
}
