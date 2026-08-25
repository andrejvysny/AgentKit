/**
 * Proposals: list a chat's staged writes, decide them, apply them.
 *
 * Every route here re-reads the record after the service call and answers with
 * the projection of what is now stored, not with what the call returned. The
 * two are the same on the happy path; when they are not — an apply whose
 * outcome was already recorded, a decision that raced another — the store is
 * the one a client should be shown.
 */
import type { ProposalDto } from "@agentkit/contracts";
import type {
  ApplyOutcome,
  ProposalRecord,
  ProposalStatus,
} from "@agentkit/host";
import { jsonResponse, readJsonObject, readPositiveInt } from "../http.js";
import { badRequest, notFound, notImplemented } from "../problem.js";
import { proposalDto } from "../projections.js";
import {
  validateApplyProposalRequest,
  validateProposalDecisionRequest,
} from "../validate.js";
import { pathParam, type RouteContext } from "./context.js";

const PROPOSAL_STATUSES: ReadonlySet<string> = new Set([
  "pending",
  "approved",
  "applying",
  "applied",
  "failed",
  "rejected",
  "invalidated",
]);

export async function listProposals(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const chat = await ctx.deps.store.conversations.getChat(chatId);
  if (chat === null) return notFound(`Chat not found: ${chatId}`, ctx.instance);

  const limit = readPositiveInt(ctx.url, "limit");
  if (!limit.ok) {
    return badRequest("invalid_request", limit.message, ctx.instance);
  }
  const status = ctx.url.searchParams.get("status");
  if (status !== null && !PROPOSAL_STATUSES.has(status)) {
    return badRequest(
      "invalid_request",
      `Query parameter \`status\` must be one of: ${[...PROPOSAL_STATUSES].join(", ")}.`,
      ctx.instance,
    );
  }

  const records = await ctx.deps.store.proposals.listByChat(chatId, {
    ...(limit.value === undefined ? {} : { limit: limit.value }),
    ...(status === null ? {} : { status: status as ProposalStatus }),
  });
  const items: ProposalDto[] = [];
  for (const record of records) items.push(await project(ctx, record));
  return jsonResponse(items);
}

export async function approveProposal(ctx: RouteContext): Promise<Response> {
  return decide(ctx, "approve");
}

export async function rejectProposal(ctx: RouteContext): Promise<Response> {
  return decide(ctx, "reject");
}

/**
 * `operationId` is the CLIENT's idempotency key for the side effect, per
 * `ApplyProposalRequest` — replaying it returns the recorded outcome and the
 * applier is never invoked twice. This route therefore does not mint one: a
 * server-minted id would make every retry a fresh apply, which is the exact
 * failure the field exists to prevent.
 */
export async function applyProposal(ctx: RouteContext): Promise<Response> {
  const service = ctx.deps.proposals;
  if (service === undefined) {
    return notImplemented(
      "This deployment was wired without a proposal service.",
      ctx.instance,
    );
  }
  const proposalId = pathParam(ctx, "proposalId");
  const existing = await ctx.deps.store.proposals.get(proposalId);
  if (existing === null) {
    return notFound(`Proposal not found: ${proposalId}`, ctx.instance);
  }

  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateApplyProposalRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }

  await service.apply({
    proposalId,
    operationId: validated.value.operationId,
  });
  const applied = await ctx.deps.store.proposals.get(proposalId);
  return jsonResponse(await project(ctx, applied ?? existing));
}

async function decide(
  ctx: RouteContext,
  kind: "approve" | "reject",
): Promise<Response> {
  const service = ctx.deps.proposals;
  if (service === undefined) {
    return notImplemented(
      "This deployment was wired without a proposal service.",
      ctx.instance,
    );
  }
  const proposalId = pathParam(ctx, "proposalId");
  const existing = await ctx.deps.store.proposals.get(proposalId);
  if (existing === null) {
    return notFound(`Proposal not found: ${proposalId}`, ctx.instance);
  }

  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateProposalDecisionRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const reason = validated.value.reason;

  // `actor: "user"` — and never `"policy"`, which must carry the `policyId`
  // that authorised it. A decision arriving over HTTP came from a person, and
  // recording it as a machine approval would make the audit trail lie about the
  // one distinction it exists to preserve.
  const decided: ProposalRecord =
    kind === "approve"
      ? await service.approve({
          proposalId,
          actor: "user",
          ...(reason === undefined ? {} : { reason }),
        })
      : await service.reject({
          proposalId,
          ...(reason === undefined ? {} : { reason }),
        });
  return jsonResponse(await project(ctx, decided));
}

/**
 * An outcome is stored against the apply's `operationId`, not on the record, so
 * the projection needs one read per proposal that has been claimed for apply.
 */
async function project(
  ctx: RouteContext,
  record: ProposalRecord,
): Promise<ProposalDto> {
  let outcome: ApplyOutcome | null = null;
  if (record.operationId !== undefined) {
    outcome = await ctx.deps.store.proposals.getOutcome(record.operationId);
  }
  return proposalDto(record, outcome);
}
