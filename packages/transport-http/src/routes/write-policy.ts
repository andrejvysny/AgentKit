/**
 * Standing write grants — the "don't ask me again for this tool in this chat"
 * half of the proposal pipeline.
 *
 * All three routes are CHAT-SCOPED, and two of them take that chat from a
 * `?chatId=` query rather than from the path. That is the port's shape showing
 * through, not a URL nobody thought about: `WritePolicy` holds grants per
 * `(chat, tool, kind)` and offers no unscoped listing, and `revoke` takes the
 * chat alongside the key precisely so one chat cannot revoke another's consent
 * by guessing a key. A route that dropped the chat would have to widen the port
 * to serve a prettier path, and the thing it would widen is the boundary around
 * a user's "yes".
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

/** The `?chatId=` every allowance route needs, or the 400 that says so. */
function requiredChatId(
  ctx: RouteContext,
): { ok: true; chatId: string } | { ok: false; response: Response } {
  const chatId = ctx.url.searchParams.get("chatId");
  if (chatId === null || chatId.trim() === "") {
    return {
      ok: false,
      response: badRequest(
        "invalid_request",
        "Query parameter `chatId` is required: a write allowance belongs to one chat.",
        ctx.instance,
      ),
    };
  }
  return { ok: true, chatId };
}

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
  const chat = requiredChatId(ctx);
  if (!chat.ok) return chat.response;
  const allowances: WriteAllowanceDto[] = policy
    .list(chat.chatId)
    .map(writeAllowanceDto);
  const body: WriteAllowanceListResponse = { allowances };
  return jsonResponse(body);
}

/**
 * Grant one, or re-state it. 201 with the allowance.
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
  const granted = policy.allow(validated.value);
  return jsonResponse(writeAllowanceDto(granted), 201);
}

/**
 * Revoke one. 204, whether or not it was there.
 *
 * The port's `revoke` is silent about a key it does not hold, and this route
 * stays silent with it: "there is no such grant" and "the grant is gone" are
 * the same state from a client's side, and a 404 would only tell a caller
 * something about a chat's consent that it could not otherwise read.
 */
export async function revokeAllowance(ctx: RouteContext): Promise<Response> {
  const policy = ctx.deps.writePolicy;
  if (policy === undefined) return noPolicy(ctx);
  const chat = requiredChatId(ctx);
  if (!chat.ok) return chat.response;
  policy.revoke(chat.chatId, pathParam(ctx, "allowanceId"));
  return new Response(null, { status: 204 });
}
