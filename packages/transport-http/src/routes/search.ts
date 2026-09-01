/**
 * Full-text search over message bodies — the one route backed by an OPTIONAL
 * store method.
 */
import type { MessageSearchResponse } from "@agentkit/contracts";
import { jsonResponse, readPositiveInt } from "../http.js";
import { badRequest, notImplemented } from "../problem.js";
import { messageSearchHitDto } from "../projections.js";
import type { RouteContext } from "./context.js";

/**
 * `?q=` (required), `?chatId=` (scope to one conversation), `?limit=`.
 *
 * **501 when the store cannot search.** `ConversationStore.searchMessages` is
 * optional — a store that cannot index text omits the method, and the port's
 * own rule is that a caller checks for it rather than catching a "not
 * supported" error. Answering 501 is that check, made where a client can see
 * it; an empty result set would be a lie a client cannot tell from "nothing
 * matched".
 *
 * A MISSING `q` is a 400, but a BLANK one is not: the port promises that a
 * query which sanitizes to nothing returns `[]` rather than raising, because
 * search boxes emit punctuation and a 500 from a stray `*` is a worse answer
 * than no results. So an absent parameter (a client bug) is refused and an
 * empty one (a user mid-type) is answered.
 *
 * Hits are NOT restricted to the active path, and that is the port's decision
 * too: a message on a branch the conversation moved away from is still
 * something that was said in this chat, and is exactly what someone searching
 * for "where did I see that?" is looking for.
 */
export async function searchMessages(ctx: RouteContext): Promise<Response> {
  const search = ctx.deps.store.conversations.searchMessages;
  if (search === undefined) {
    return notImplemented(
      "This deployment's conversation store does not implement message search.",
      ctx.instance,
    );
  }

  const query = ctx.url.searchParams.get("q");
  if (query === null) {
    return badRequest(
      "invalid_request",
      "Query parameter `q` is required.",
      ctx.instance,
    );
  }

  const limit = readPositiveInt(ctx.url, "limit");
  if (!limit.ok) {
    return badRequest("invalid_request", limit.message, ctx.instance);
  }
  const chatId = ctx.url.searchParams.get("chatId");

  const hits = await search.call(ctx.deps.store.conversations, query, {
    ...(chatId === null || chatId === "" ? {} : { chatId }),
    ...(limit.value === undefined ? {} : { limit: limit.value }),
  });
  const body: MessageSearchResponse = { hits: hits.map(messageSearchHitDto) };
  return jsonResponse(body);
}
