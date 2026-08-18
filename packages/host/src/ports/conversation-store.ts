import type { AiToolCall } from "@agentkit/contracts";

/** A conversation thread. Chats own messages; runs are attached to a chat. */
export interface ChatRecord {
  id: string;
  title?: string;
  createdAt: string;
  updatedAt: string;
  metadata: Record<string, unknown>;
}

/**
 * One persisted message.
 *
 * `orderKey` — not `createdAt` — is the ordering key inside a chat, for the same
 * reason `seq` orders events: several messages are written in one transaction
 * within the same millisecond, and timestamps cannot separate them.
 *
 * `runId` marks the run that produced the message, and is what
 * `orderMessagesForProvider` groups on when it repairs provider-legal ordering
 * (internal assistant turn → its tool results → the visible answer).
 *
 * Reserved `metadata` keys the host layer itself reads:
 * - `internal: true` — a replay-only record (the assistant turn carrying
 *   `toolCalls`, and the tool results answering them). Not shown as chat.
 * - `placeholder: true` — the empty assistant message created at submit time and
 *   filled in as the run streams.
 * - `toolName` — for `role: "tool"` records, the tool that produced the result.
 * - `banner` — a `role: "system"` record the UI renders as a warning banner.
 */
export interface MessageRecord {
  id: string;
  chatId: string;
  runId?: string;
  role: "user" | "assistant" | "tool" | "system";
  content: string;
  /** Monotonic within a chat; assigned by {@link ConversationStore.appendMessage}. */
  orderKey: number;
  toolCallId?: string;
  toolCalls?: AiToolCall[];
  /**
   * The slim, model-facing envelope for a tool result. History replays THIS to
   * the provider; the full payload stays in the run event log.
   */
  modelResultJson?: string;
  metadata: Record<string, unknown>;
  createdAt: string;
}

export interface CreateChatInput {
  id?: string;
  title?: string;
  metadata?: Record<string, unknown>;
}

export interface AppendMessageInput {
  id?: string;
  chatId: string;
  runId?: string;
  role: MessageRecord["role"];
  content: string;
  toolCallId?: string;
  toolCalls?: AiToolCall[];
  modelResultJson?: string;
  metadata?: Record<string, unknown>;
}

/**
 * Fields a message may change after it was written. Deliberately narrow: a
 * streaming answer grows its `content`, a finished run rewrites `metadata`, and
 * nothing else about a persisted message is mutable.
 *
 * `metadata` REPLACES the stored bag rather than merging into it — a merge makes
 * "unset this flag" unexpressible.
 */
export interface UpdateMessagePatch {
  content?: string;
  metadata?: Record<string, unknown>;
  toolCalls?: AiToolCall[];
}

export interface ListMessagesOptions {
  /** Most recent N messages (still returned in ascending `orderKey`). */
  limit?: number;
  /** Only messages with `orderKey` greater than this. */
  afterOrderKey?: number;
}

export interface ListChatsOptions {
  limit?: number;
  /** Chats updated before this ISO timestamp (keyset pagination). */
  before?: string;
}

export interface ConversationStore {
  createChat(input: CreateChatInput): Promise<ChatRecord>;
  getChat(chatId: string): Promise<ChatRecord | null>;
  listChats(opts?: ListChatsOptions): Promise<ChatRecord[]>;
  /**
   * Append a message, assigning the next `orderKey` for its chat. Assigning the
   * key in the store (not the caller) is what makes concurrent appends from a
   * run and from a user land in a defined order.
   */
  appendMessage(input: AppendMessageInput): Promise<MessageRecord>;
  updateMessage(
    messageId: string,
    patch: UpdateMessagePatch,
  ): Promise<MessageRecord>;
  /** Ascending `orderKey`. */
  listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]>;
}
