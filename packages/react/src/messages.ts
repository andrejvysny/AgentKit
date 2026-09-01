/**
 * Reading a chat's active path, and the optimistic records that stand in for it
 * until the server answers.
 */
import type { AgentKitClient } from "@agentkit/client";
import type { MessageDto, SubmitMessageRequest } from "@agentkit/contracts";

/** How much of a conversation a single load will follow. */
export interface PagingOptions {
  /** `limit` for each `listMessages` call. Omit to take the server's default (100). */
  pageSize?: number;
  /**
   * How many pages to follow before stopping. Default 20.
   *
   * The cap is not politeness — it is the difference between a slow chat and a
   * hung one. `listMessages` pages FORWARD from the oldest message, so the
   * newest turn is on the LAST page: a hook that stopped after page one would
   * render a conversation missing the message the user just sent. Following to
   * the end is therefore the only correct default, and the cap is what stops a
   * pathological chat from turning one render into an unbounded fetch loop.
   */
  maxPages?: number;
}

export const DEFAULT_MAX_PAGES = 20;

/**
 * The chat's whole active path, oldest first.
 *
 * The path — not every message in the chat. Off-path siblings are what
 * `listSiblings` is for; a list that mixed them in would render a conversation
 * that was never had.
 */
export async function loadActivePath(
  client: AgentKitClient,
  chatId: string,
  paging: PagingOptions = {},
  signal?: AbortSignal,
): Promise<MessageDto[]> {
  const maxPages = paging.maxPages ?? DEFAULT_MAX_PAGES;
  const items: MessageDto[] = [];
  let cursor: string | undefined;

  for (let page = 0; page < maxPages; page += 1) {
    const answer = await client.listMessages(
      {
        chatId,
        ...(paging.pageSize === undefined ? {} : { limit: paging.pageSize }),
        ...(cursor === undefined ? {} : { cursor }),
      },
      signal === undefined ? undefined : { signal },
    );
    items.push(...answer.items);
    if (answer.nextCursor === undefined) return items;
    cursor = answer.nextCursor;
  }
  return items;
}

/** Ids for one optimistic turn. Never sent anywhere; replaced by the server's. */
let optimisticCounter = 0;
export function nextOptimisticIds(): { user: string; assistant: string } {
  optimisticCounter += 1;
  return {
    user: `optimistic-user-${optimisticCounter}`,
    assistant: `optimistic-assistant-${optimisticCounter}`,
  };
}

/**
 * The two records a submit will create, as the server will create them.
 *
 * `metadata.optimistic` is this package's own marker, alongside the host's
 * reserved `placeholder`: a component that wants to grey out an unconfirmed
 * bubble needs to tell "the server has not answered yet" apart from "the answer
 * is still streaming", and only the first of those is a state the server has
 * never heard of.
 */
export function optimisticPair(input: {
  chatId: string;
  ids: { user: string; assistant: string };
  content: SubmitMessageRequest["content"];
  metadata?: Record<string, unknown>;
}): { user: MessageDto; assistant: MessageDto } {
  const createdAt = new Date().toISOString();
  return {
    user: {
      id: input.ids.user,
      chatId: input.chatId,
      role: "user",
      content: input.content,
      metadata: { ...(input.metadata ?? {}), optimistic: true },
      createdAt,
    },
    assistant: {
      id: input.ids.assistant,
      chatId: input.chatId,
      role: "assistant",
      content: "",
      metadata: { optimistic: true, placeholder: true },
      createdAt,
    },
  };
}

/**
 * The list a branch submit optimistically leaves behind: everything up to and
 * including `parentMessageId`.
 *
 * Edit-and-regenerate cuts the conversation at the parent — the answer that
 * followed the old question is not on the new branch — so a list that only
 * APPENDED would show the user their rewritten question below the answer to the
 * one they replaced. An unknown parent leaves the list alone: the truncation is
 * a guess about a server write that has not happened yet, and guessing a chat
 * empty is worse than guessing it unchanged.
 */
export function truncateAfter(
  messages: readonly MessageDto[],
  parentMessageId: string,
): MessageDto[] {
  const at = messages.findIndex((message) => message.id === parentMessageId);
  if (at === -1) return [...messages];
  return messages.slice(0, at + 1);
}

/** Replace a message by id, leaving the list alone when it holds no such id. */
export function replaceMessage(
  messages: readonly MessageDto[],
  id: string,
  change: (message: MessageDto) => MessageDto,
): MessageDto[] {
  return messages.map((message) =>
    message.id === id ? change(message) : message,
  );
}

/** Append `delta` to a message whose content is a plain string. */
export function appendDelta(message: MessageDto, delta: string): MessageDto {
  if (typeof message.content !== "string") return message;
  return { ...message, content: message.content + delta };
}
