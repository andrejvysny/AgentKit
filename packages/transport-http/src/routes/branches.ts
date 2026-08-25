/**
 * Branching: the three routes that exist because a chat is a tree.
 *
 * `listMessages` answers with one path through that tree, which is the only
 * shape that reads as a conversation. These are how a client sees the rest of
 * it: what other answers exist at a point (`listSiblings`), which one the chat
 * is showing (`activateBranch`), and how to take a prefix somewhere new
 * (`forkChat`).
 *
 * None of them validates the tree itself. Every rule about what a legal fork
 * point is, or what "activate" does to the flags, belongs to `ConversationStore`
 * and is enforced inside its transaction; a second opinion here would be a
 * second place that could disagree with storage — and the one that answered
 * first would win, which is the wrong one.
 */
import type { ChatDto, MessageDto, MessagePageDto } from "@agentkit/contracts";
import { jsonResponse, readJsonObject } from "../http.js";
import { badRequest, notFound } from "../problem.js";
import { chatDto, messageDto } from "../projections.js";
import { pathParam, type RouteContext } from "./context.js";
import { validateForkChatRequest } from "../validate.js";

/**
 * Copy a chat's active path, up to a message, into a new chat. 201 with the new
 * chat.
 *
 * A missing source chat is the transport's own 404; an illegal fork point is
 * not — `forkChat` raises `invalid_fork_point`, which the problem table maps to
 * 400, and re-deciding here what counts as on-path would mean reading the tree
 * twice and trusting the read that cannot see the transaction.
 */
export async function forkChat(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateForkChatRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }

  const chat = await ctx.deps.store.conversations.getChat(chatId);
  if (chat === null) return notFound(`Chat not found: ${chatId}`, ctx.instance);

  const result = await ctx.deps.store.conversations.forkChat(
    chatId,
    validated.value.fromMessageId,
  );
  const dto: ChatDto = chatDto(result.chat);
  return jsonResponse(dto, 201);
}

/**
 * Make a message's branch the chat's active path, and answer with that path.
 *
 * Answering with the path — rather than 204 — is what makes this one round trip
 * for the client that just switched branches: it asked for a different
 * conversation and it gets the conversation, instead of an acknowledgement plus
 * a follow-up `listMessages` whose result could already be a turn behind.
 *
 * The path comes back FROM the switch rather than from a `listMessages` after
 * it. `activatePath` already computes the set it is about to write, so the
 * read-back would be a second, weaker answer: it leaves the store's
 * transaction, so a concurrent append could move the path between the write and
 * the read and this route would report a conversation that was never the result
 * of this call. An unknown message never reaches the projection at all —
 * the store raises `not_found`, which the problem mapper turns into a 404, and
 * a pre-flight existence check here would just be a second opinion about a
 * question storage answers inside the transaction.
 *
 * There is no cursor on the response — a branch switch returns the whole path,
 * and paging it would mean paging a conversation the client has not seen yet.
 */
export async function activateBranch(ctx: RouteContext): Promise<Response> {
  const messageId = pathParam(ctx, "messageId");
  const rows = await ctx.deps.store.conversations.activatePath(messageId);
  const page: MessagePageDto = { items: rows.map(messageDto) };
  return jsonResponse(page);
}

/** The message's siblings, itself included, `branchIndex` ascending. */
export async function listSiblings(ctx: RouteContext): Promise<Response> {
  const messageId = pathParam(ctx, "messageId");
  const siblings = await ctx.deps.store.conversations.listSiblings(messageId);
  const items: MessageDto[] = siblings.map(messageDto);
  return jsonResponse(items);
}
