/**
 * Standing write grants — the "don't ask me again for this tool in this chat"
 * half of the proposal pipeline.
 *
 * All three routes are NESTED UNDER THE CHAT (`/v1/chats/:chatId/…`), and that
 * is the port's shape and the authorizer's requirement agreeing with each
 * other. `WritePolicy` holds grants per `(chat, tool, kind)` and offers no
 * unscoped listing, and `revoke` takes the chat alongside the key precisely so
 * one chat cannot revoke another's consent by guessing a key — while an
 * `AuthorizationPort` is handed the path and the URL and never the body, so a
 * chat carried anywhere else could not be gated per conversation at all.
 *
 * **501 without `deps.writePolicy`.** Wiring it is a deliberate decision:
 * `SessionWritePolicy` keeps grants in memory for the life of the process
 * BECAUSE consent given in a conversation someone is watching should not
 * outlive it, and a route that grants one is a route anything past
 * `authenticate` can call.
 */
import type {
  WriteAllowanceDto,
  WriteAllowanceListResponse,
} from "@agentkit/contracts";
import { jsonResponse, readJsonObject } from "../http.js";
import { badRequest, notImplemented } from "../problem.js";
import { writeAllowanceDto } from "../projections.js";
import { pathParam, type RouteContext } from "./context.js";
import { validateGrantAllowanceRequest } from "../validate.js";

/** The 501 all three share. */
function noPolicy(ctx: RouteContext): Response {
  return notImplemented(
    "This deployment does not expose write-policy allowances; no WritePolicy is wired.",
    ctx.instance,
  );
}

export async function listAllowances(ctx: RouteContext): Promise<Response> {
  const policy = ctx.deps.writePolicy;
  if (policy === undefined) return noPolicy(ctx);
  const allowances: WriteAllowanceDto[] = policy
    .list(pathParam(ctx, "chatId"))
    .map(writeAllowanceDto);
  const body: WriteAllowanceListResponse = { allowances };
  return jsonResponse(body);
}

/**
 * Grant one, or re-state it. 201 with the allowance.
 *
 * The chat comes from the PATH, and the body carries only what the grant is
 * about — a `chatId` in the body would be the one field deciding whose consent
 * this is, and the one field the authorizer that ran before this handler never
 * saw.
 *
 * Always a 201, never a 200-on-replay: `WritePolicy.allow` is an upsert keyed
 * on `(chat, tool, kind)`, so re-granting overwrites the ceiling rather than
 * creating a second grant — and the route has nothing to compare against to
 * tell the two apart. There is no side effect to duplicate either, which is why
 * this is the one write in the API that needs no idempotency key.
 */
export async function grantAllowance(ctx: RouteContext): Promise<Response> {
  const policy = ctx.deps.writePolicy;
  if (policy === undefined) return noPolicy(ctx);
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateGrantAllowanceRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  // Named fields, path `chatId` LAST: the body must never be able to say which
  // chat a grant lands in — that is the one field the authorizer checked.
  const granted = policy.allow({
    toolName: validated.value.toolName,
    proposalKind: validated.value.proposalKind,
    maxRisk: validated.value.maxRisk,
    chatId: pathParam(ctx, "chatId"),
  });
  return jsonResponse(writeAllowanceDto(granted), 201);
}

/**
 * Revoke one. 204, whether or not it was there.
 *
 * The port's `revoke` is silent about a key it does not hold, and this route
 * stays silent with it: "there is no such grant" and "the grant is gone" are
 * the same state from a client's side, and a 404 would only tell a caller
 * something about a chat's consent that it could not otherwise read. A key
 * belonging to ANOTHER chat is that same silence — the path names the chat, and
 * the port refuses to cross it.
 */
export async function revokeAllowance(ctx: RouteContext): Promise<Response> {
  const policy = ctx.deps.writePolicy;
  if (policy === undefined) return noPolicy(ctx);
  policy.revoke(pathParam(ctx, "chatId"), pathParam(ctx, "allowanceId"));
  return new Response(null, { status: 204 });
}
