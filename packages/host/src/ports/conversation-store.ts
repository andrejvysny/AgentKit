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
 * ── A CHAT IS A TREE, AND THE ACTIVE PATH IS A PER-MESSAGE FLAG ────────────
 *
 * `parentMessageId` makes the messages of a chat a forest (one root in the
 * normal case), and `active` marks which root-to-leaf path through it the
 * conversation currently IS. Storing the path as a flag on every message —
 * rather than as a pointer to a leaf, or a path table — is what makes
 * "give me the conversation" a single indexed read (`WHERE active ORDER BY
 * depth`) instead of a walk, and it is the representation this design is
 * copied from because it survived production there.
 *
 * The invariants, all maintained by {@link ConversationStore} and never by a
 * caller:
 *
 * - The active messages of a chat form exactly one chain from a root to a
 *   CHILDLESS leaf. Every append and every {@link ConversationStore.activatePath}
 *   lands on that shape or does nothing.
 * - A child's `orderKey` is always greater than its parent's, because a parent
 *   must exist before a child can name it. Along any root-to-leaf chain, then,
 *   `depth` and `orderKey` agree — which is why `(depth, orderKey)` ordering and
 *   plain `orderKey` ordering are the same order on the active path, and why the
 *   `afterOrderKey` cursor still pages forward correctly through a branched chat.
 * - A chat nobody has branched is a straight line: every message has
 *   `branchIndex: 0`, `depth` counts up by one, and everything is `active`. That
 *   is the degenerate case, and every method below behaves on it exactly as it
 *   did before branching existed.
 *
 * `orderKey` — not `createdAt` — is the append-order key inside a chat, for the
 * same reason `seq` orders events: several messages are written in one
 * transaction within the same millisecond, and timestamps cannot separate them.
 * With a tree above it, its job narrows to breaking ties WITHIN a depth (and to
 * being the paging cursor); the tree, not the counter, decides who follows whom.
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
  /** The message this one answers. Absent on a root. */
  parentMessageId?: string;
  /** Distance from the root. `0` on a root; `parent.depth + 1` otherwise. */
  depth: number;
  /**
   * Position among the messages sharing this parent, ascending.
   *
   * `0` is the first child written; each later branch under the same parent
   * takes the next index and keeps it forever, so a client can name "the second
   * answer to this question" and still mean the same message next week.
   */
  branchIndex: number;
  /** Whether this message is on the chat's currently active path. */
  active: boolean;
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
  /**
   * Where to hang this message. The field is what turns a linear append into a
   * branch, and the two cases are deliberately not two methods:
   *
   * - **Absent** — the parent is the chat's current active leaf. That is the
   *   append every existing caller already makes, and on a chat nobody has
   *   branched it produces exactly the straight line it always did: `depth`
   *   counting up, `branchIndex: 0`, nothing to re-activate.
   * - **Present** — a NEW BRANCH under the named message. `branchIndex` becomes
   *   one past the highest already used among its children, the new message is
   *   created ACTIVE, and the whole path switches to it in the same write.
   *
   * The append-and-activate is ONE operation rather than an append followed by
   * an {@link ConversationStore.activatePath} because this is the submit path:
   * a window in which the branch exists but nothing is active is a window in
   * which "what is this conversation?" has no answer, and a crash inside it
   * leaves a chat that reads as empty.
   *
   * A message in another chat, or one that does not exist, is a
   * `RecordNotFoundError` — a parent is a structural claim, not a hint.
   */
  parentMessageId?: string;
  metadata?: Record<string, unknown>;
}

/**
 * Fields a message may change after it was written. Deliberately narrow: a
 * streaming answer grows its `content`, a finished run rewrites `metadata`, and
 * nothing else about a persisted message is mutable — its position in the tree
 * least of all, which is why `parentMessageId`, `depth`, `branchIndex` and
 * `active` are absent here. Moving a message between branches after the fact
 * would rewrite what a model was already shown; branching creates a new message
 * instead.
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
  /** Most recent N messages (still returned in ascending path order). */
  limit?: number;
  /** Only messages with `orderKey` greater than this. */
  afterOrderKey?: number;
}

export interface ListChatsOptions {
  limit?: number;
  /** Chats updated before this ISO timestamp (keyset pagination). */
  before?: string;
}

/** What {@link ConversationStore.forkChat} wrote: the new chat and its copy. */
export interface ForkChatResult {
  chat: ChatRecord;
  /** The copied messages, root first — the fork's whole active path. */
  messages: MessageRecord[];
}

export interface ConversationStore {
  createChat(input: CreateChatInput): Promise<ChatRecord>;
  getChat(chatId: string): Promise<ChatRecord | null>;
  listChats(opts?: ListChatsOptions): Promise<ChatRecord[]>;
  /**
   * Append a message, assigning the next `orderKey` for its chat and placing it
   * in the chat's tree (see {@link AppendMessageInput.parentMessageId}).
   * Assigning the key in the store (not the caller) is what makes concurrent
   * appends from a run and from a user land in a defined order.
   *
   * A branching append — one that names a parent — is ATOMIC with the path
   * switch it causes: either the branch exists and is active, or neither.
   */
  appendMessage(input: AppendMessageInput): Promise<MessageRecord>;
  updateMessage(
    messageId: string,
    patch: UpdateMessagePatch,
  ): Promise<MessageRecord>;
  /**
   * The chat's ACTIVE PATH, root first, ordered `(depth ASC, orderKey ASC)`.
   *
   * Not every message in the chat: off-path branches are exactly the answers the
   * conversation is not currently giving, and replaying them to a provider — or
   * rendering them inline — would produce a transcript that never happened. A
   * chat nobody has branched has one path, so this returns byte-identically what
   * it returned before branching existed.
   *
   * `limit` still means "the most recent N", and `afterOrderKey` still pages
   * forward; both apply to the active path, and both remain well-defined because
   * `orderKey` increases with depth along any path (see {@link MessageRecord}).
   */
  listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]>;
  /**
   * The messages sharing a message's parent, INCLUDING the message itself,
   * ordered by `branchIndex` ascending. For a root, the chat's roots.
   *
   * Self-inclusive because the caller is a branch switcher: "which answers to
   * this question exist, and which am I looking at?" is one question, and a list
   * that omitted the current one would make the second half unanswerable.
   */
  listSiblings(messageId: string): Promise<MessageRecord[]>;
  /**
   * Make a message's path the chat's active one, atomically.
   *
   * What becomes active: the message, every ancestor of it, and then a descent
   * to a leaf — at each step preferring the child that is ALREADY active, and
   * falling back to the lowest `branchIndex` when none is. Preferring the active
   * child is what makes switching branches feel like memory: return to a branch
   * you were reading and you land where you left it, not at its first answer.
   * Everything else in the chat becomes inactive.
   *
   * Atomic, and that is the whole point of it being a port method rather than a
   * loop over `updateMessage`: a half-applied switch is a chat with two active
   * leaves, or none, and `listMessages` cannot report either as a conversation.
   */
  activatePath(messageId: string): Promise<void>;
  /**
   * Copy a chat's active path, up to and including `fromMessageId`, into a NEW
   * chat — in one transaction.
   *
   * `fromMessageId` MUST be on the source chat's active path; unknown, in
   * another chat, or off-path all raise `InvalidForkPointError`. The copy is
   * flattened: fresh ids, parents remapped onto the copies, the first message a
   * root, every `branchIndex` back to `0`, `depth` recounted `0..n`, everything
   * active. So a fork is a straight line again, and branching it later starts a
   * fresh tree that owes the original nothing.
   *
   * What is deliberately NOT carried over:
   * - `runId` on every copied message. The runs belong to the source chat's task
   *   log; a copy pointing at them would make a run look like it wrote messages
   *   in two conversations.
   * - Any message whose `metadata.placeholder` is true — an answer still
   *   streaming is not history, and copying one would freeze an empty assistant
   *   turn into the fork forever. The `placeholder` flag itself is dropped from
   *   whatever metadata is copied.
   *
   * What IS carried over, against the instinct to drop it: `internal: true`
   * records. The fork has to be able to replay tool calls and their results to
   * the provider, and a copy missing them leaves the model looking at answers
   * with no evidence behind them.
   */
  forkChat(chatId: string, fromMessageId: string): Promise<ForkChatResult>;
}
