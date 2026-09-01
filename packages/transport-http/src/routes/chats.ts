/**
 * Chats and messages: the conversation half of the contract.
 *
 * `submitMessage` is the only write in the API that creates three records at
 * once (a task, a user message, an assistant placeholder), and it is the reason
 * `Idempotency-Key` is mandatory rather than advisory — see below.
 */
import type { ChatDto, MessagePageDto } from "@agentkit/contracts";
import { jsonResponse, readJsonObject, readPositiveInt } from "../http.js";
import {
  deriveIdempotentTaskId,
  deriveRegenerateTaskId,
  IDEMPOTENCY_KEY_HEADER,
} from "../idempotency.js";
import { decodeMessageCursor, encodeMessageCursor } from "../cursor.js";
import { badRequest, notFound, notImplemented } from "../problem.js";
import { chatDto, messageDto } from "../projections.js";
import { pathParam, type RouteContext } from "./context.js";
import {
  validateCreateChatRequest,
  validateRegenerateMessageRequest,
  validateSubmitMessageRequest,
  validateUpdateChatRequest,
} from "../validate.js";

/** Page size when a client names none. */
const DEFAULT_MESSAGE_PAGE_SIZE = 100;

export async function createChat(ctx: RouteContext): Promise<Response> {
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateCreateChatRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const chat = await ctx.deps.store.conversations.createChat({
    ...(validated.value.title === undefined
      ? {}
      : { title: validated.value.title }),
    ...(validated.value.metadata === undefined
      ? {}
      : { metadata: validated.value.metadata }),
  });
  return jsonResponse(chatDto(chat), 201);
}

export async function listChats(ctx: RouteContext): Promise<Response> {
  const limit = readPositiveInt(ctx.url, "limit");
  if (!limit.ok) {
    return badRequest("invalid_request", limit.message, ctx.instance);
  }
  const before = ctx.url.searchParams.get("before");
  const chats = await ctx.deps.store.conversations.listChats({
    ...(limit.value === undefined ? {} : { limit: limit.value }),
    ...(before === null ? {} : { before }),
  });
  const items: ChatDto[] = chats.map(chatDto);
  return jsonResponse(items);
}

export async function getChat(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const chat = await ctx.deps.store.conversations.getChat(chatId);
  if (chat === null) return notFound(`Chat not found: ${chatId}`, ctx.instance);
  return jsonResponse(chatDto(chat));
}

/**
 * One page of messages, oldest first.
 *
 * The page is sliced HERE rather than pushed down as `ListMessagesOptions.limit`
 * because that option means "the most recent N" — the right primitive for
 * building a provider prompt, the wrong one for paging forward through a
 * conversation, which is what this route does. A store call with `afterOrderKey`
 * and a local head-slice is the only combination that yields forward pages.
 */
export async function listMessages(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const chat = await ctx.deps.store.conversations.getChat(chatId);
  if (chat === null) return notFound(`Chat not found: ${chatId}`, ctx.instance);

  const limit = readPositiveInt(ctx.url, "limit");
  if (!limit.ok) {
    return badRequest("invalid_request", limit.message, ctx.instance);
  }
  const pageSize = limit.value ?? DEFAULT_MESSAGE_PAGE_SIZE;

  const rawCursor = ctx.url.searchParams.get("cursor");
  let afterOrderKey: number | undefined;
  if (rawCursor !== null) {
    const decoded = decodeMessageCursor(rawCursor);
    if (decoded === null) {
      return badRequest(
        "invalid_request",
        "Query parameter `cursor` is not a cursor this server issued.",
        ctx.instance,
      );
    }
    afterOrderKey = decoded;
  }

  const rows = await ctx.deps.store.conversations.listMessages(chatId, {
    ...(afterOrderKey === undefined ? {} : { afterOrderKey }),
  });
  const page = rows.slice(0, pageSize);
  const last = page[page.length - 1];
  const body: MessagePageDto = {
    items: page.map(messageDto),
    ...(rows.length > page.length && last !== undefined
      ? { nextCursor: encodeMessageCursor(last.orderKey) }
      : {}),
  };
  return jsonResponse(body);
}

/**
 * Submit a turn. 201 the first time, 200 on a replay of the same key.
 *
 * The key is MANDATORY, and a missing one is a 400 rather than a generated id:
 * a client that retries a timed-out POST without one duplicates the turn, and
 * nothing on this side can tell that retry apart from a user who really did
 * send twice. Refusing is the only answer that cannot answer someone twice.
 *
 * The 201/200 split is decided by reading the task row BEFORE submitting.
 * `TurnRunner.submitMessage` is idempotent either way — it returns the first
 * submit's ids and re-pokes the queue — but it does not report WHICH happened,
 * and the pre-read is the cheapest way to find out. Two genuinely concurrent
 * first submits can both read "absent" and both answer 201 with the same body;
 * that is a duplicated status code, not a duplicated turn.
 */
export async function submitMessage(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const key = ctx.req.headers.get(IDEMPOTENCY_KEY_HEADER);
  if (key === null || key.trim() === "") {
    return badRequest(
      "idempotency_key_required",
      "Submitting a message requires an `Idempotency-Key` header.",
      ctx.instance,
    );
  }

  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateSubmitMessageRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }

  // Checked before the task row is written: `submitMessage` creates the task
  // first inside its transaction, and a store whose `transaction` is only a
  // logical grouping would leave that row behind when the message append
  // failed on a chat that does not exist.
  const chat = await ctx.deps.store.conversations.getChat(chatId);
  if (chat === null) return notFound(`Chat not found: ${chatId}`, ctx.instance);

  // Same reasoning one level down for a branch submit: a `parentMessageId` in
  // ANOTHER chat is the mistake this pre-check exists for, since the store's own
  // guard would fire mid-transaction, behind the task row. `listSiblings` is the
  // read that answers it — it includes the message itself, and raises
  // `not_found` (a 404) for an id nothing has.
  const parentMessageId = validated.value.parentMessageId;
  if (parentMessageId !== undefined) {
    const siblings =
      await ctx.deps.store.conversations.listSiblings(parentMessageId);
    const parent = siblings.find((record) => record.id === parentMessageId);
    if (parent === undefined || parent.chatId !== chatId) {
      return notFound(
        `Message not found in chat ${chatId}: ${parentMessageId}`,
        ctx.instance,
      );
    }
  }

  const taskId = await deriveIdempotentTaskId(chatId, key.trim());
  const existing = await ctx.deps.store.tasks.getTask(taskId);

  const result = await ctx.deps.turns.submitMessage({
    chatId,
    content: validated.value.content,
    ...(validated.value.model === undefined
      ? {}
      : { model: validated.value.model }),
    ...(parentMessageId === undefined ? {} : { parentMessageId }),
    ...(validated.value.metadata === undefined
      ? {}
      : { metadata: validated.value.metadata }),
    taskId,
  });

  return jsonResponse(result, existing === null ? 201 : 200);
}

/**
 * Rename, re-tag, archive or unarchive. 200 with the updated {@link ChatDto}.
 *
 * The store's own `updateChat` raises `not_found` for an unknown chat, so there
 * is no pre-flight read here: a check plus a write is two round trips to answer
 * the same question the write already answers, and the window between them is a
 * window where the answer can change.
 *
 * `metadata` REPLACES the stored bag; that is the port's rule, and this route
 * neither merges nor warns about it.
 */
export async function updateChat(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateUpdateChatRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const patch = validated.value;
  const chat = await ctx.deps.store.conversations.updateChat(chatId, {
    ...(patch.title === undefined ? {} : { title: patch.title }),
    ...(patch.metadata === undefined ? {} : { metadata: patch.metadata }),
    ...(patch.archived === undefined ? {} : { archived: patch.archived }),
  });
  return jsonResponse(chatDto(chat));
}

/**
 * Delete the chat and everything the host holds about it — 204, no body.
 *
 * Routed through `ConversationService` rather than `ConversationStore`, and
 * that is not a detail: a chat's remains live in three stores (messages, the
 * runs that executed in its scope with their event logs, the proposals it
 * staged), and the service is what deletes all three in ONE transaction and
 * what refuses — `chat_busy`, 409 — while a run in the chat is still live. A
 * route that called the conversation store directly would leave orphan runs
 * behind and delete a conversation out from under a provider call.
 *
 * 501 without the service, like every other optional dependency: the route is
 * in the contract and another deployment of this version serves it.
 */
export async function deleteChat(ctx: RouteContext): Promise<Response> {
  const conversations = ctx.deps.conversations;
  if (conversations === undefined) {
    return notImplemented(
      "This deployment does not serve chat deletion; no ConversationService is wired.",
      ctx.instance,
    );
  }
  await conversations.deleteChat(pathParam(ctx, "chatId"));
  return new Response(null, { status: 204 });
}

/**
 * Answer the same question again, on a new branch. 201 the first time, 200 on
 * a replay of the same key — the same split, decided the same way, as
 * {@link submitMessage}.
 *
 * The key is MANDATORY here for the same reason it is there: this creates a run
 * and a message, and a client retrying a timed-out POST without one gets a
 * second answer to a question it only meant to re-ask once. The derived task id
 * carries its own prefix, so a submit and a regenerate under one key can never
 * land on each other's run (see `idempotency.ts`).
 *
 * Both existence checks are the host's: `TurnRunner.regenerate` looks the
 * target up before it writes anything, and raises `not_found` (unknown, or in
 * another chat) or `invalid_regenerate` (a question, a replay-only record, a
 * root). Repeating them here would be a second copy of a rule that already
 * has one home.
 */
export async function regenerateMessage(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const messageId = pathParam(ctx, "messageId");
  const key = ctx.req.headers.get(IDEMPOTENCY_KEY_HEADER);
  if (key === null || key.trim() === "") {
    return badRequest(
      "idempotency_key_required",
      "Regenerating a message requires an `Idempotency-Key` header.",
      ctx.instance,
    );
  }

  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateRegenerateMessageRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }

  const taskId = await deriveRegenerateTaskId(chatId, messageId, key.trim());
  const existing = await ctx.deps.store.tasks.getTask(taskId);

  const result = await ctx.deps.turns.regenerate({
    chatId,
    messageId,
    ...(validated.value.model === undefined
      ? {}
      : { model: validated.value.model }),
    ...(validated.value.providerId === undefined
      ? {}
      : { providerId: validated.value.providerId }),
    ...(validated.value.metadata === undefined
      ? {}
      : { metadata: validated.value.metadata }),
    taskId,
  });

  return jsonResponse(result, existing === null ? 201 : 200);
}
