/**
 * Map-backed, complete {@link AssistantStore}: every port method enforces the
 * same invariants the sqlite adapter enforces (transition legality,
 * `(scopeKey, actionId)` uniqueness, outcome idempotency, seq monotonicity,
 * lease/fencing semantics) — a store an embedding host can run against
 * directly for local dev, a single-process deployment, or a test harness that
 * does not want a database file.
 *
 * `packages/host/tests/fakes.ts` has a smaller version of this idea (Maps,
 * `transaction` runs the callback) built only to exercise host's own unit
 * tests. This file is the real thing: complete against every port method, and
 * it is graded by `@agentkit/testing`'s store-conformance suite rather than by
 * what one service's tests happen to touch.
 *
 * SNAPSHOT RETURNS: every record handed back to a caller — from a create, a
 * read, or a transition — is a shallow copy, never the object living inside
 * this store's Maps. `SqliteAssistantStore` gets this for free (it rebuilds
 * every record from a freshly-read row); a Map-backed store has to do it on
 * purpose, or a caller holding an old `Lease`/`TaskRecord`/... would see it
 * silently mutate later when a DIFFERENT call (e.g. `renewLease` bumping
 * `expiresAt` on the same stored object) touches the same record.
 */
import type {
  AiContentPart,
  AiMessageContent,
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
  TaskEventEnvelope,
} from "@agentkit/contracts";
import {
  ACTION_ID_RELEASING_STATUSES,
  activationSetOf,
  activeLeafOf,
  activePathOf,
  assertListMessagesCursors,
  DEFAULT_SEARCH_LIMIT,
  DuplicateActionIdError,
  DuplicateTaskError,
  InvalidImportError,
  InvalidProposalTransitionError,
  InvalidTaskTransitionError,
  isSearchableMetadata,
  LeaseLostError,
  RecordNotFoundError,
  SeqConflictError,
  UnknownDependencyError,
  assertProposalTransition,
  assertTaskTransition,
  defaultClock,
  defaultIds,
  evaluateTaskDependencies,
  forkedChatTitle,
  forkPrefixOf,
  assertAppendActivation,
  hasActiveChild,
  nextBranchIndex,
  planForkedMessages,
  planImportedMessages,
  searchTextOf,
  SEARCH_MATCH_END,
  SEARCH_MATCH_START,
  SEARCH_SNIPPET_ELLIPSIS,
  siblingsOf,
  type AppendEventsOptions,
  type AppendMessageInput,
  type ApplyOutcome,
  type AssistantSettings,
  type AssistantStore,
  type AttemptRecord,
  type AcquireLeaseInput,
  type ChatRecord,
  type Clock,
  type ClaimNextInput,
  type ClaimedTask,
  type ConversationStore,
  type CreateAttemptInput,
  type CreateChatInput,
  type CreateProposalInput,
  type CreateTaskInput,
  type EndAttemptInput,
  type ForkChatResult,
  type IdGenerator,
  type ImportConversationInput,
  type Lease,
  type ListChatsOptions,
  type ListEventsOptions,
  type ListMessagesOptions,
  type ListProposalsOptions,
  type MessageRecord,
  type MessageSearchHit,
  type SearchMessagesOptions,
  type UpdateChatPatch,
  type OutboxAppendInput,
  type OutboxClaimInput,
  type OutboxRecord,
  type OutboxStore,
  type ProposalPatch,
  type ProposalRecord,
  type ProposalStatus,
  type ProposalStore,
  type ProviderStore,
  type SettingsStore,
  type TaskDependencyState,
  type TaskPatch,
  type TaskRecord,
  type TaskStatus,
  type TaskStore,
  type UpdateMessagePatch,
  type UpdateProgressOptions,
  effectivePriority,
  resolveTaskAging,
  type ResolvedTaskAging,
  type TaskAgingOptions,
} from "@agentkit/host";

/** Lease TTL {@link MemoryTaskStore.claimNext} grants the attempt it creates. */
const DEFAULT_LEASE_TTL_MS = 30_000;
/** How long a claimed-but-unresolved outbox record stays invisible to `claimBatch`. */
const DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS = 30_000;

export interface MemoryAssistantStoreOptions extends TaskAgingOptions {
  /** Defaults to {@link defaultClock} (real wall-clock). */
  clock?: Clock;
  /** Defaults to {@link defaultIds} (UUID-backed). */
  ids?: IdGenerator;
  /** Lease TTL `claimNext` grants the attempt it creates. Default 30s. */
  leaseTtlMs?: number;
  /** Outbox claim-visibility window. Default 30s. */
  outboxClaimVisibilityMs?: number;
}

/**
 * A message body copied deeply enough that nothing a caller does to it can reach
 * this store's Maps — and nothing this store does can reach a body a caller
 * already holds.
 *
 * A string needs no copy (immutable). A parts array does: the SNAPSHOT RETURNS
 * rule at the top of this file is a SHALLOW copy of the record, which would hand
 * out the very array object the store keeps — so a caller appending a part, or
 * rewriting a `source`, would silently edit history. `SqliteAssistantStore` gets
 * this for free by rebuilding parts from JSON on every read; a Map-backed store
 * has to mean it.
 *
 * Two levels is exactly enough, and not by luck: a part is `{ type, text }` or
 * `{ type, source, detail? }`, and `source` is the only nested object the CLOSED
 * part union can hold (`packages/contracts/src/content.ts`). A part kind with
 * deeper structure is an additive change to that union — and to this function.
 */
function copyMessageContent(content: AiMessageContent): AiMessageContent {
  if (typeof content === "string") return content;
  return content.map(copyContentPart);
}

function copyContentPart(part: AiContentPart): AiContentPart {
  return part.type === "text"
    ? { ...part }
    : { ...part, source: { ...part.source } };
}

/** The snapshot copy of a message record: shallow, plus a real content copy. */
function copyMessage(record: MessageRecord): MessageRecord {
  return { ...record, content: copyMessageContent(record.content) };
}

/**
 * ATOMICITY, IN A STORE WITH NO TRANSACTIONS: every method below that touches
 * more than one record — a branching append, {@link activatePath}, and
 * {@link forkChat} — validates and computes EVERYTHING first, then applies its
 * mutations in one straight-line synchronous block with no `await` in it.
 *
 * That is enough here, and only here. These methods are `async` to satisfy the
 * port, but a JavaScript function that never awaits runs to completion before
 * any other task can observe it, so there is no interleaving to protect against
 * and no half-applied path switch a concurrent `listMessages` could read. What
 * it does NOT give is crash-consistency or rollback — a throw partway through
 * the apply would still leave the Maps changed, which is exactly why the apply
 * phase is the part that cannot throw, and why
 * `capabilities.atomicTransactions: false` still tells the conformance suite the
 * truth about this adapter.
 */
export class MemoryConversationStore implements ConversationStore {
  readonly chats = new Map<string, ChatRecord>();
  private readonly messagesById = new Map<string, MessageRecord>();
  private readonly messagesByChat = new Map<string, MessageRecord[]>();
  private readonly orderKeys = new Map<string, number>();

  constructor(
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
  ) {}

  async createChat(input: CreateChatInput): Promise<ChatRecord> {
    const now = this.clock.nowIso();
    const chat: ChatRecord = {
      id: input.id ?? this.ids.chatId(),
      ...(input.title === undefined ? {} : { title: input.title }),
      createdAt: now,
      updatedAt: now,
      metadata: input.metadata ?? {},
      archived: false,
    };
    this.chats.set(chat.id, chat);
    this.messagesByChat.set(chat.id, []);
    return { ...chat };
  }

  async getChat(chatId: string): Promise<ChatRecord | null> {
    const chat = this.chats.get(chatId);
    return chat ? { ...chat } : null;
  }

  async listChats(opts?: ListChatsOptions): Promise<ChatRecord[]> {
    // `ids` is an explicit batch fetch, so it resolves archived chats too —
    // archiving hides a chat from BROWSING, not from a caller that can already
    // name it. An empty array is "none of these", never "no filter".
    const wanted = opts?.ids === undefined ? undefined : new Set(opts.ids);
    let rows = [...this.chats.values()]
      .filter((c) =>
        wanted === undefined
          ? opts?.includeArchived === true || !c.archived
          : wanted.has(c.id),
      )
      .sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
    if (opts?.before !== undefined) {
      const before = opts.before;
      rows = rows.filter((c) => c.updatedAt < before);
    }
    if (opts?.limit !== undefined) rows = rows.slice(0, opts.limit);
    // Copies: `chats` holds live records that keep mutating (updatedAt on
    // every appendMessage) — a caller holding onto a listed row must not see
    // it change under it later.
    return rows.map((c) => ({ ...c }));
  }

  async updateChat(
    chatId: string,
    patch: UpdateChatPatch,
  ): Promise<ChatRecord> {
    const chat = this.chats.get(chatId);
    if (chat === undefined) {
      throw new RecordNotFoundError(`Chat not found: ${chatId}`);
    }
    if (patch.title !== undefined) chat.title = patch.title;
    // Metadata REPLACES the stored bag, per the port contract.
    if (patch.metadata !== undefined) chat.metadata = patch.metadata;
    if (patch.archived !== undefined) chat.archived = patch.archived;
    chat.updatedAt = this.clock.nowIso();
    return { ...chat };
  }

  /**
   * Drop the chat and every message in it — off-path branches included.
   *
   * One synchronous block after the existence check, so there is no interleaving
   * point at which the chat is gone but its messages are not (see the class doc
   * on ATOMICITY).
   */
  async deleteChat(chatId: string): Promise<void> {
    if (!this.chats.has(chatId)) {
      throw new RecordNotFoundError(`Chat not found: ${chatId}`);
    }
    for (const message of this.messagesByChat.get(chatId) ?? []) {
      this.messagesById.delete(message.id);
    }
    this.messagesByChat.delete(chatId);
    this.orderKeys.delete(chatId);
    this.chats.delete(chatId);
  }

  async appendMessage(input: AppendMessageInput): Promise<MessageRecord> {
    assertAppendActivation(input);
    const chat = this.chats.get(input.chatId);
    if (chat === undefined) {
      throw new RecordNotFoundError(`Chat not found: ${input.chatId}`);
    }
    const list = this.messagesByChat.get(input.chatId) ?? [];
    // An explicitly named parent is a structural claim and is checked as one;
    // an absent one means "continue the conversation", which is the active leaf.
    const parent =
      input.parentMessageId === undefined
        ? activeLeafOf(list)
        : this.requireParent(input.chatId, input.parentMessageId);
    const parentId = parent?.id;
    // A CHAIN append: place the record and stop. `assertAppendActivation` has
    // already proved a parent was named, so the flag comes from a message the
    // caller pointed at rather than from whichever branch happens to be live.
    const chained = input.activate === false;
    const orderKey = (this.orderKeys.get(input.chatId) ?? 0) + 1;
    const now = this.clock.nowIso();
    const record: MessageRecord = {
      id: input.id ?? this.ids.messageId(),
      chatId: input.chatId,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      role: input.role,
      // Copied on the way IN as well as out: the caller keeps its own array,
      // and a submit that reused one across two appends must not alias them.
      content: copyMessageContent(input.content),
      orderKey,
      ...(input.toolCallId === undefined
        ? {}
        : { toolCallId: input.toolCallId }),
      ...(input.toolCalls === undefined ? {} : { toolCalls: input.toolCalls }),
      ...(input.modelResultJson === undefined
        ? {}
        : { modelResultJson: input.modelResultJson }),
      ...(parentId === undefined ? {} : { parentMessageId: parentId }),
      depth: parent === undefined ? 0 : parent.depth + 1,
      // Computed BEFORE the push, or the record would count itself as its own
      // sibling and every append would land on index 1.
      branchIndex: nextBranchIndex(list, parentId),
      active: chained ? chainedActive(list, parent) : true,
      metadata: input.metadata ?? {},
      createdAt: now,
    };
    this.orderKeys.set(input.chatId, orderKey);
    this.messagesById.set(record.id, record);
    list.push(record);
    this.messagesByChat.set(input.chatId, list);
    // Run unconditionally rather than only for a branching append. On the
    // append-to-the-active-leaf path the new record is the leaf's only active
    // descendant, so the computed set is the old path plus this record and every
    // flag it writes is the flag already there — the same answer, by the same
    // code, which is one fewer special case to get wrong. A chain append is the
    // one caller that skips it: it inherited its flag from its parent and is
    // explicitly not asking for the path to move.
    if (!chained) applyActivation(list, record.id);
    chat.updatedAt = now;
    return copyMessage(record);
  }

  /** The named parent, proven to exist and to be in the same chat. */
  private requireParent(
    chatId: string,
    parentMessageId: string,
  ): MessageRecord {
    const parent = this.messagesById.get(parentMessageId);
    if (parent === undefined || parent.chatId !== chatId) {
      throw new RecordNotFoundError(
        `Parent message not found in chat ${chatId}: ${parentMessageId}`,
      );
    }
    return parent;
  }

  async updateMessage(
    messageId: string,
    patch: UpdateMessagePatch,
  ): Promise<MessageRecord> {
    const record = this.messagesById.get(messageId);
    if (!record) {
      throw new RecordNotFoundError(`Message not found: ${messageId}`);
    }
    if (patch.content !== undefined) {
      record.content = copyMessageContent(patch.content);
    }
    // Metadata REPLACES the stored bag, per the port contract.
    if (patch.metadata !== undefined) record.metadata = patch.metadata;
    if (patch.toolCalls !== undefined) record.toolCalls = patch.toolCalls;
    return copyMessage(record);
  }

  /** The chat's ACTIVE PATH, `(depth, orderKey)` ascending — see the port. */
  async listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]> {
    assertListMessagesCursors(opts);
    let rows = activePathOf(this.messagesByChat.get(chatId) ?? []);
    if (opts?.afterOrderKey !== undefined) {
      const after = opts.afterOrderKey;
      rows = rows.filter((m) => m.orderKey > after);
    }
    if (opts?.beforeOrderKey !== undefined) {
      const before = opts.beforeOrderKey;
      rows = rows.filter((m) => m.orderKey < before);
    }
    // `slice(-limit)` for BOTH cursors, and that is not an oversight: the page
    // a scroll-back wants is the LAST `limit` messages before the key — the
    // ones nearest it — exactly as a plain listing wants the last `limit` in
    // the chat.
    if (opts?.limit !== undefined) rows = rows.slice(-opts.limit);
    return rows.map(copyMessage);
  }

  async listSiblings(messageId: string): Promise<MessageRecord[]> {
    const record = this.requireMessage(messageId);
    const list = this.messagesByChat.get(record.chatId) ?? [];
    return siblingsOf(list, record).map(copyMessage);
  }

  async activatePath(messageId: string): Promise<MessageRecord[]> {
    const record = this.requireMessage(messageId);
    const list = this.messagesByChat.get(record.chatId) ?? [];
    applyActivation(list, messageId);
    // Read off the flags this call just wrote, in the same synchronous block —
    // so what the caller gets back is the path as of the switch, not as of
    // whenever it might have re-read the chat.
    return activePathOf(list).map(copyMessage);
  }

  /**
   * Copy the source chat's active path prefix into a new chat.
   *
   * Read, validate and BUILD first; write last. The build phase is where every
   * throw lives (`invalid_fork_point` from {@link forkPrefixOf}, a missing
   * chat), so by the time the first Map is touched nothing is left that can
   * fail — which is how a store with no rollback still cannot leave a
   * half-copied chat behind.
   */
  async forkChat(
    chatId: string,
    fromMessageId: string,
  ): Promise<ForkChatResult> {
    const source = this.chats.get(chatId);
    if (source === undefined) {
      throw new RecordNotFoundError(`Chat not found: ${chatId}`);
    }
    const prefix = forkPrefixOf(
      this.messagesByChat.get(chatId) ?? [],
      chatId,
      fromMessageId,
    );
    const plans = planForkedMessages(prefix, () => this.ids.messageId());
    const now = this.clock.nowIso();
    const title = forkedChatTitle(source.title);
    const chat: ChatRecord = {
      id: this.ids.chatId(),
      ...(title === undefined ? {} : { title }),
      createdAt: now,
      updatedAt: now,
      // The chat's own metadata IS copied (its labels, its owner, whatever the
      // host keeps there); only per-MESSAGE run linkage is stripped, and a fork
      // that lost the conversation's own bookkeeping would be a different chat
      // rather than a copy of this one.
      metadata: { ...source.metadata },
      // NOT inherited: a fork is a conversation somebody just started, and
      // starting one already filed away is not a state a user can have meant.
      archived: false,
    };
    const messages: MessageRecord[] = plans.map((plan, index) => ({
      id: plan.id,
      chatId: chat.id,
      role: plan.source.role,
      content: copyMessageContent(plan.source.content),
      orderKey: index + 1,
      ...(plan.source.toolCallId === undefined
        ? {}
        : { toolCallId: plan.source.toolCallId }),
      ...(plan.source.toolCalls === undefined
        ? {}
        : { toolCalls: [...plan.source.toolCalls] }),
      ...(plan.source.modelResultJson === undefined
        ? {}
        : { modelResultJson: plan.source.modelResultJson }),
      ...(plan.parentMessageId === undefined
        ? {}
        : { parentMessageId: plan.parentMessageId }),
      depth: plan.depth,
      branchIndex: 0,
      active: true,
      metadata: plan.metadata,
      createdAt: now,
    }));

    this.chats.set(chat.id, chat);
    this.messagesByChat.set(chat.id, messages);
    for (const message of messages) this.messagesById.set(message.id, message);
    this.orderKeys.set(chat.id, messages.length);
    return { chat: { ...chat }, messages: messages.map(copyMessage) };
  }

  /**
   * Write a whole conversation with the caller's ids, in one go.
   *
   * VALIDATE AND BUILD FIRST, WRITE LAST — the same discipline `forkChat`
   * follows, and the only thing that makes "all-or-nothing" true in a store
   * with no rollback. Every rejection lives in the duplicate-chat check and in
   * `planImportedMessages`; by the time the first Map is touched, nothing left
   * to do can throw.
   */
  async importConversation(
    input: ImportConversationInput,
  ): Promise<ChatRecord> {
    if (this.chats.has(input.chat.id)) {
      throw new InvalidImportError(
        `Cannot import chat ${input.chat.id}: a chat with that id already exists.`,
        { reason: "duplicate_chat", chatId: input.chat.id },
      );
    }
    const createdAt = input.chat.createdAt ?? this.clock.nowIso();
    const plans = planImportedMessages(
      input.messages,
      input.chat.id,
      createdAt,
    );
    const chat: ChatRecord = {
      id: input.chat.id,
      ...(input.chat.title === undefined ? {} : { title: input.chat.title }),
      createdAt,
      // `createdAt`, not now: an import of a year-old conversation that jumped
      // to the top of the chat list would reorder the whole history it was
      // meant to preserve. A host that wants it at the top passes a fresh
      // `createdAt`.
      updatedAt: createdAt,
      metadata: input.chat.metadata ?? {},
      archived: input.chat.archived ?? false,
    };
    const messages: MessageRecord[] = plans.map((plan) => ({
      id: plan.input.id,
      chatId: chat.id,
      role: plan.input.role,
      content: copyMessageContent(plan.input.content),
      orderKey: plan.orderKey,
      ...(plan.parentMessageId === undefined
        ? {}
        : { parentMessageId: plan.parentMessageId }),
      depth: plan.depth,
      branchIndex: plan.branchIndex,
      active: plan.input.active,
      metadata: plan.metadata,
      createdAt: plan.createdAt,
    }));

    this.chats.set(chat.id, chat);
    this.messagesByChat.set(chat.id, messages);
    for (const message of messages) this.messagesById.set(message.id, message);
    this.orderKeys.set(chat.id, messages.length);
    return { ...chat };
  }

  /**
   * Case-insensitive SUBSTRING search over every message body in the store.
   *
   * A scan, not an index, and honestly so: this adapter exists for local dev, a
   * single-process host and tests, where the whole conversation history is
   * already in memory and a real inverted index would be a second thing to keep
   * correct for no gain a user of it could measure. `SqliteAssistantStore` is
   * where FTS5 and bm25 live.
   *
   * Ranking is therefore an approximation with a stated rule rather than a
   * relevance model: more occurrences first, then the most recent message. That
   * is enough for the property the port actually promises — the best match is
   * not buried — and pretending to more would be inventing a score nothing
   * computes.
   */
  async searchMessages(
    query: string,
    opts?: SearchMessagesOptions,
  ): Promise<MessageSearchHit[]> {
    const needle = query.trim().toLowerCase();
    if (needle.length === 0) return [];
    const limit = opts?.limit ?? DEFAULT_SEARCH_LIMIT;
    const scored: { hit: MessageSearchHit; count: number; order: number }[] =
      [];
    for (const [chatId, list] of this.messagesByChat) {
      if (opts?.chatId !== undefined && opts.chatId !== chatId) continue;
      for (const message of list) {
        if (!isSearchableMetadata(message.metadata)) continue;
        // ALL text parts, never just the first — see `searchTextOf`.
        const text = searchTextOf(message.content);
        const at = text.toLowerCase().indexOf(needle);
        if (at === -1) continue;
        scored.push({
          hit: {
            chatId,
            messageId: message.id,
            snippet: snippetAround(text, at, needle.length),
          },
          count: countOccurrences(text.toLowerCase(), needle),
          order: message.orderKey,
        });
      }
    }
    scored.sort((a, b) => b.count - a.count || b.order - a.order);
    return scored.slice(0, limit).map((entry) => entry.hit);
  }

  private requireMessage(messageId: string): MessageRecord {
    const record = this.messagesById.get(messageId);
    if (record === undefined) {
      throw new RecordNotFoundError(`Message not found: ${messageId}`);
    }
    return record;
  }
}

/**
 * The `active` flag a CHAIN append inherits: true only when the parent is
 * active AND still the end of the live chain — see the port's
 * `AppendMessageInput.activate`.
 */
function chainedActive(
  records: readonly MessageRecord[],
  parent: MessageRecord | undefined,
): boolean {
  if (parent === undefined || !parent.active) return false;
  return !hasActiveChild(records, parent.id);
}

/**
 * Rewrite every `active` flag in one chat so `messageId`'s path is the live one.
 *
 * Mutates the stored records in place, which is safe precisely because every
 * read path in this class hands back copies (see the class doc on SNAPSHOT
 * RETURNS): nobody outside is holding one of these objects expecting it to stay
 * still.
 */
function applyActivation(
  records: readonly MessageRecord[],
  messageId: string,
): void {
  const active = activationSetOf(records, messageId);
  for (const record of records) record.active = active.has(record.id);
}

/** How much text a snippet keeps on either side of the match. */
const SNIPPET_CONTEXT_CHARS = 40;

/**
 * A window of `text` around the match at `at`, with the match marked and
 * anything cut off replaced by the shared ellipsis.
 *
 * FIRST match only, because that is what a substring search knows: it stopped
 * looking the moment it found one. FTS5's `snippet()` marks every term inside
 * the window it picks, so the two adapters produce different snippets for the
 * same message — which is why the port promises the MARKERS and the presence of
 * the matched text, not a byte-identical string.
 */
function snippetAround(text: string, at: number, length: number): string {
  const start = Math.max(0, at - SNIPPET_CONTEXT_CHARS);
  const end = Math.min(text.length, at + length + SNIPPET_CONTEXT_CHARS);
  return [
    start > 0 ? SEARCH_SNIPPET_ELLIPSIS : "",
    text.slice(start, at),
    SEARCH_MATCH_START,
    text.slice(at, at + length),
    SEARCH_MATCH_END,
    text.slice(at + length, end),
    end < text.length ? SEARCH_SNIPPET_ELLIPSIS : "",
  ].join("");
}

/** Non-overlapping occurrences of `needle` in `haystack` (both lowercased). */
function countOccurrences(haystack: string, needle: string): number {
  let count = 0;
  let from = 0;
  for (;;) {
    const at = haystack.indexOf(needle, from);
    if (at === -1) return count;
    count += 1;
    from = at + needle.length;
  }
}

export class MemoryTaskStore implements TaskStore {
  readonly tasks = new Map<string, TaskRecord>();
  readonly attempts = new Map<string, AttemptRecord>();
  /** One live lease per task, keyed by taskId — mirrors the sqlite `leases` PK. */
  private readonly leases = new Map<string, Lease>();
  private readonly leasesByToken = new Map<string, string>();
  private readonly events = new Map<string, TaskEventEnvelope[]>();
  /** Store-global monotonic fencing counter — every `acquireLease` draws the next value. */
  private fencing = 0;

  private readonly aging: ResolvedTaskAging;

  constructor(
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
    private readonly leaseTtlMs: number = DEFAULT_LEASE_TTL_MS,
    aging: TaskAgingOptions = {},
  ) {
    this.aging = resolveTaskAging(aging);
  }

  async createTask(input: CreateTaskInput): Promise<TaskRecord> {
    // Never overwrite: the id is the caller's idempotency key, and replacing a
    // live row would strand its attempts, its lease and its event log.
    if (this.tasks.has(input.taskId)) {
      throw new DuplicateTaskError(`Task already exists: ${input.taskId}.`, {
        taskId: input.taskId,
      });
    }
    if (
      input.parentTaskId !== undefined &&
      !this.tasks.has(input.parentTaskId)
    ) {
      throw new UnknownDependencyError(
        `Task ${input.taskId} names parent ${input.parentTaskId}, which does not exist.`,
        { taskId: input.taskId, parentTaskId: input.parentTaskId },
      );
    }
    for (const dependency of input.dependsOn ?? []) {
      // Self-dependency is checked first and by identity, not by lookup: the
      // row is not in the Map yet, so a plain existence check would report the
      // wrong reason for a task that waits on itself.
      if (dependency === input.taskId || !this.tasks.has(dependency)) {
        throw new UnknownDependencyError(
          `Task ${input.taskId} depends on ${dependency}, which does not exist.`,
          { taskId: input.taskId, dependsOn: dependency },
        );
      }
    }
    const now = this.clock.nowIso();
    const task: TaskRecord = {
      taskId: input.taskId,
      kind: input.kind,
      scopeId: input.scopeId,
      status: "queued",
      priority: input.priority ?? 0,
      enqueuedAt: now,
      availableAt: input.availableAt ?? now,
      payload: input.payload,
      ...(input.parentTaskId === undefined
        ? {}
        : { parentTaskId: input.parentTaskId }),
      // Copied, not aliased: `dependsOn` is immutable after create, and holding
      // the caller's array would let them edit the gate after the fact.
      ...(input.dependsOn === undefined
        ? {}
        : { dependsOn: [...input.dependsOn] }),
      attemptCount: 0,
      poisonCount: 0,
    };
    this.tasks.set(task.taskId, task);
    return { ...task };
  }

  async getTask(taskId: string): Promise<TaskRecord | null> {
    const task = this.tasks.get(taskId);
    return task ? { ...task } : null;
  }

  async listChildren(taskId: string): Promise<TaskRecord[]> {
    return [...this.tasks.values()]
      .filter((task) => task.parentTaskId === taskId)
      .map((task) => ({ ...task }));
  }

  async listByScope(scopeId: string): Promise<TaskRecord[]> {
    return [...this.tasks.values()]
      .filter((task) => task.scopeId === scopeId)
      .map((task) => ({ ...task }));
  }

  /**
   * Drop every task in a scope with its attempts, its lease and its events.
   *
   * The token index is cleared alongside the lease it points at: a lease token
   * left behind would keep resolving to a task that no longer exists, and the
   * writer holding it would get a `not_found` where the honest answer is
   * `lease_lost`.
   */
  async deleteByScope(scopeId: string): Promise<number> {
    const doomed = [...this.tasks.values()].filter(
      (task) => task.scopeId === scopeId,
    );
    for (const task of doomed) {
      for (const [attemptId, attempt] of this.attempts) {
        if (attempt.taskId === task.taskId) this.attempts.delete(attemptId);
      }
      const lease = this.leases.get(task.taskId);
      if (lease !== undefined) this.leasesByToken.delete(lease.leaseToken);
      this.leases.delete(task.taskId);
      this.events.delete(task.taskId);
      this.tasks.delete(task.taskId);
    }
    return doomed.length;
  }

  async transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
  ): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    if (!from.includes(task.status)) {
      throw new InvalidTaskTransitionError(
        `Task ${taskId} is ${task.status}, expected one of [${from.join(", ")}].`,
        { taskId, current: task.status, from, to },
      );
    }
    assertTaskTransition(task.status, to);
    task.status = to;
    if (patch?.startedAt !== undefined) task.startedAt = patch.startedAt;
    if (patch?.finishedAt !== undefined) task.finishedAt = patch.finishedAt;
    if (patch?.error !== undefined) task.error = patch.error;
    if (patch?.availableAt !== undefined) task.availableAt = patch.availableAt;
    if (patch?.priority !== undefined) task.priority = patch.priority;
    if (patch?.poisonCount !== undefined) task.poisonCount = patch.poisonCount;
    if (patch?.payload !== undefined) task.payload = patch.payload;
    return { ...task };
  }

  async createAttempt(input: CreateAttemptInput): Promise<AttemptRecord> {
    const task = this.tasks.get(input.taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${input.taskId}`);
    task.attemptCount += 1;
    const attempt: AttemptRecord = {
      attemptId: input.attemptId,
      taskId: input.taskId,
      attemptNumber: task.attemptCount,
      status: "running",
      ownerId: input.ownerId,
      startedAt: this.clock.nowIso(),
    };
    this.attempts.set(attempt.attemptId, attempt);
    return { ...attempt };
  }

  async endAttempt(input: EndAttemptInput): Promise<AttemptRecord> {
    const attempt = this.attempts.get(input.attemptId);
    if (!attempt) {
      throw new RecordNotFoundError(`Attempt not found: ${input.attemptId}`);
    }
    attempt.status = input.status;
    attempt.endedAt = this.clock.nowIso();
    if (input.error !== undefined) attempt.error = input.error;
    return { ...attempt };
  }

  async acquireLease(input: AcquireLeaseInput): Promise<Lease> {
    // `leases` is one row per task (mirrors the sqlite PK on task_id):
    // acquiring always mints a fresh lease and replaces whatever was there,
    // live or expired. Nothing here re-checks "is the existing lease still
    // live" — the queued->running CAS on the task (transitionTask) plus
    // claimNext only ever selecting queued tasks is what prevents two workers
    // from both believing they hold the current lease, not a check in this
    // method.
    const old = this.leases.get(input.taskId);
    if (old) this.leasesByToken.delete(old.leaseToken);
    this.fencing += 1;
    const lease: Lease = {
      taskId: input.taskId,
      attemptId: input.attemptId,
      ownerId: input.ownerId,
      leaseToken: `lease_${crypto.randomUUID()}`,
      fencingToken: this.fencing,
      expiresAt: new Date(
        this.clock.now().getTime() + input.ttlMs,
      ).toISOString(),
    };
    this.leases.set(input.taskId, lease);
    this.leasesByToken.set(lease.leaseToken, input.taskId);
    return { ...lease };
  }

  async renewLease(leaseToken: string, ttlMs: number): Promise<Lease> {
    const lease = this.currentLeaseByToken(leaseToken);
    lease.expiresAt = new Date(
      this.clock.now().getTime() + ttlMs,
    ).toISOString();
    return { ...lease };
  }

  async releaseLease(leaseToken: string): Promise<void> {
    const lease = this.currentLeaseByToken(leaseToken);
    this.leases.delete(lease.taskId);
    this.leasesByToken.delete(lease.leaseToken);
  }

  async expireStaleLeases(now: Date): Promise<Lease[]> {
    const expired: Lease[] = [];
    for (const lease of this.leases.values()) {
      if (new Date(lease.expiresAt).getTime() <= now.getTime()) {
        expired.push(lease);
      }
    }
    for (const lease of expired) {
      this.leases.delete(lease.taskId);
      this.leasesByToken.delete(lease.leaseToken);
    }
    return expired.map((l) => ({ ...l }));
  }

  async appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void> {
    const lease = this.leases.get(taskId);
    if (!lease || lease.leaseToken !== opts.leaseToken) {
      throw new LeaseLostError(
        `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
        { taskId, leaseToken: opts.leaseToken },
      );
    }
    if (events.length === 0) return;
    const log = this.events.get(taskId) ?? [];
    // Validate the WHOLE batch before mutating anything — a mid-batch seq
    // conflict must leave the store exactly as it was, not half-applied.
    let last = log.length > 0 ? log[log.length - 1]!.seq : -1;
    for (const event of events) {
      if (event.seq <= last) {
        throw new SeqConflictError(
          `Non-monotonic seq ${event.seq} for task ${taskId} (last ${last}).`,
          { taskId, seq: event.seq, last },
        );
      }
      last = event.seq;
    }
    log.push(...events);
    this.events.set(taskId, log);
  }

  async listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]> {
    let log = [...(this.events.get(taskId) ?? [])];
    if (opts?.afterSeq !== undefined) {
      const after = opts.afterSeq;
      log = log.filter((e) => e.seq > after);
    }
    if (opts?.limit !== undefined) log = log.slice(0, opts.limit);
    return log;
  }

  async nextSeq(taskId: string): Promise<number> {
    const log = this.events.get(taskId);
    if (!log || log.length === 0) return 0;
    return log[log.length - 1]!.seq + 1;
  }

  async updateProgress(
    taskId: string,
    progress: Record<string, unknown>,
    opts: UpdateProgressOptions,
  ): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    // The same ownership proof `appendEvents` demands, for the same reason: a
    // fenced-out worker must not overwrite the live attempt's snapshot.
    const lease = this.leases.get(taskId);
    if (!lease || lease.leaseToken !== opts.leaseToken) {
      throw new LeaseLostError(
        `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
        { taskId, leaseToken: opts.leaseToken },
      );
    }
    // Overwrite, and store a copy — a caller that keeps mutating the object it
    // reported would otherwise keep editing the stored snapshot.
    task.progress = { ...progress };
    return { ...task };
  }

  async claimNext(input: ClaimNextInput): Promise<ClaimedTask | null> {
    const busy = new Set(input.scopesBusy);
    const kinds = input.kinds === undefined ? null : new Set(input.kinds);
    const nowMs = input.now.getTime();
    const candidates = [...this.tasks.values()].filter(
      (task) =>
        task.status === "queued" &&
        !busy.has(task.scopeId) &&
        (kinds === null || kinds.has(task.kind)) &&
        new Date(task.availableAt).getTime() <= nowMs,
    );
    if (candidates.length === 0) return null;
    // Array.prototype.sort is stable (guaranteed since ES2019), so ties on
    // BOTH effective priority and enqueuedAt fall back to Map insertion
    // (creation) order — a deterministic FIFO even when timestamps collide.
    candidates.sort((a, b) => {
      const prioA = this.effectivePriorityOf(a, nowMs);
      const prioB = this.effectivePriorityOf(b, nowMs);
      if (prioA !== prioB) return prioB - prioA;
      return (
        new Date(a.enqueuedAt).getTime() - new Date(b.enqueuedAt).getTime()
      );
    });
    // Walk the candidates rather than taking the head: a task at the front of
    // the queue can be un-runnable (a dependency still in flight) or doomed (a
    // dependency that failed), and neither may hide the claimable work behind
    // it. See TaskStore.claimNext on why the settle happens lazily, here.
    //
    // The candidate list is a SNAPSHOT, and every transition below is an
    // `await` another caller's claimNext can interleave with — so by the time
    // a candidate's turn comes it may already have been settled or claimed by
    // that other caller. Losing the queued-> CAS is that race resolving
    // normally, not a fault: skip the candidate and keep walking. Anything
    // else still propagates.
    for (const candidate of candidates) {
      const verdict = evaluateTaskDependencies(
        this.dependencyStates(candidate),
      );
      if (verdict.kind === "blocked") continue;
      if (verdict.kind === "settle") {
        try {
          await this.transitionTask(candidate.taskId, ["queued"], verdict.to, {
            finishedAt: this.clock.nowIso(),
            ...(verdict.error === undefined ? {} : { error: verdict.error }),
          });
        } catch (err) {
          if (!(err instanceof InvalidTaskTransitionError)) throw err;
        }
        continue;
      }
      // CAPTURED BEFORE THE FIRST WRITE, because everything from here on is one
      // claim and a claim is all-or-nothing. `claimNext` promises an ATOMIC
      // claim, and the promise is about FAILURE — success looks atomic in any
      // implementation. The sqlite adapter gets this from `BEGIN IMMEDIATE`; a
      // Map-backed store has to undo by hand, and the cost of not doing so is
      // not a stale field but a LOST TASK: `running` with no lease is work
      // nothing can finish (no worker holds a token), nothing can re-claim
      // (there is no `running → queued` edge) and no recovery can see
      // (`expireStaleLeases` reports leases, and there would be none).
      const undo = this.captureClaimUndo(candidate.taskId);
      let task: TaskRecord;
      try {
        task = await this.transitionTask(
          candidate.taskId,
          ["queued"],
          "running",
          { startedAt: this.clock.nowIso() },
        );
      } catch (err) {
        // A lost CAS wrote nothing, so there is nothing to undo — skip the
        // candidate and keep walking. Anything else did not necessarily stop
        // that cleanly.
        if (!(err instanceof InvalidTaskTransitionError)) {
          undo();
          throw err;
        }
        continue;
      }
      try {
        const attempt = await this.createAttempt({
          attemptId: this.ids.attemptId(),
          taskId: task.taskId,
          ownerId: input.ownerId,
        });
        const lease = await this.acquireLease({
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: input.ownerId,
          ttlMs: this.leaseTtlMs,
        });
        return { task, attempt, lease };
      } catch (err) {
        undo();
        throw err;
      }
    }
    return null;
  }

  async markDeadLettered(taskId: string, reason: string): Promise<TaskRecord> {
    const task = this.tasks.get(taskId);
    if (!task) throw new RecordNotFoundError(`Task not found: ${taskId}`);
    task.deadLetteredAt = this.clock.nowIso();
    task.deadLetterReason = reason;
    return { ...task };
  }

  /**
   * Snapshot everything a claim is about to touch, and hand back the function
   * that puts it all back — this store's stand-in for the sqlite adapter's
   * ROLLBACK.
   *
   * Written against the private Maps rather than through the port on purpose:
   * the public path back would be a `running → queued` transition, and that
   * edge does not exist ({@link TASK_TRANSITIONS} omits it precisely so a
   * started task cannot be handed to a second worker). Undoing a claim that was
   * never completed is not that transition — nobody ever saw the row as
   * `running`, because the claim that wrote it is the same call that is now
   * unwinding — so this restores the fields directly instead of asking the
   * state machine to permit something it is right to forbid.
   */
  private captureClaimUndo(taskId: string): () => void {
    const task = this.tasks.get(taskId);
    const before = {
      status: task?.status,
      startedAt: task?.startedAt,
      attemptCount: task?.attemptCount,
    };
    const attemptsBefore = new Set(this.attempts.keys());
    const fencingBefore = this.fencing;
    const hadLease = this.leases.has(taskId);
    return () => {
      const current = this.tasks.get(taskId);
      if (current && before.status !== undefined) {
        current.status = before.status;
        if (before.startedAt === undefined) delete current.startedAt;
        else current.startedAt = before.startedAt;
        current.attemptCount = before.attemptCount ?? 0;
      }
      // Any attempt row this claim managed to write.
      for (const attemptId of this.attempts.keys()) {
        if (!attemptsBefore.has(attemptId)) this.attempts.delete(attemptId);
      }
      // A lease this claim minted. Only ever one, and only when there was none
      // before — a claim takes a `queued` task, which by definition has no
      // lease to preserve.
      if (!hadLease) {
        const lease = this.leases.get(taskId);
        if (lease) {
          this.leases.delete(taskId);
          this.leasesByToken.delete(lease.leaseToken);
        }
      }
      // The fencing counter goes back ONLY if nothing else drew from it in the
      // meantime. Handing token N to a real lease and then rewinding past it
      // would break the one property the token has — and a skipped value costs
      // nothing, since the contract is that tokens increase, not that they are
      // dense.
      if (this.fencing === fencingBefore + 1) this.fencing = fencingBefore;
    };
  }

  private effectivePriorityOf(task: TaskRecord, nowMs: number): number {
    return effectivePriority(
      task.priority,
      new Date(task.enqueuedAt).getTime(),
      nowMs,
      this.aging,
    );
  }

  /** The narrow projection {@link evaluateTaskDependencies} grades. */
  private dependencyStates(task: TaskRecord): TaskDependencyState[] {
    return (task.dependsOn ?? []).map((dependencyId) => {
      const dependency = this.tasks.get(dependencyId);
      return {
        taskId: dependencyId,
        status: dependency?.status ?? null,
        deadLettered: dependency?.deadLetteredAt !== undefined,
      };
    });
  }

  private currentLeaseByToken(leaseToken: string): Lease {
    const taskId = this.leasesByToken.get(leaseToken);
    if (taskId !== undefined) {
      const lease = this.leases.get(taskId);
      if (lease && lease.leaseToken === leaseToken) return lease;
    }
    throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
      leaseToken,
    });
  }
}

export class MemoryProposalStore implements ProposalStore {
  readonly proposals = new Map<string, ProposalRecord>();
  /** Insertion order, so `getByActionId` can walk it backwards for "most recent". */
  private readonly proposalOrder: string[] = [];
  readonly outcomes = new Map<string, ApplyOutcome>();

  constructor(private readonly clock: Clock) {}

  async create(input: CreateProposalInput): Promise<ProposalRecord> {
    if (input.actionId !== undefined) {
      const actionId = input.actionId;
      const clash = [...this.proposals.values()].find(
        (p) =>
          p.scopeKey === input.scopeKey &&
          p.actionId === actionId &&
          !ACTION_ID_RELEASING_STATUSES.includes(p.status),
      );
      if (clash) {
        throw new DuplicateActionIdError(
          `action_id ${actionId} already used in scope ${input.scopeKey}.`,
          { scopeKey: input.scopeKey, actionId },
        );
      }
    }
    const record: ProposalRecord = {
      id: input.id,
      chatId: input.chatId,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      scopeKey: input.scopeKey,
      ...(input.actionId === undefined ? {} : { actionId: input.actionId }),
      toolName: input.toolName,
      kind: input.kind,
      risk: input.risk,
      status: "pending",
      envelope: input.envelope,
      operations: input.operations,
      warnings: input.warnings,
      truncated: input.truncated,
      ...(input.revisionAtCreate === undefined
        ? {}
        : { revisionAtCreate: input.revisionAtCreate }),
      createdAt: input.createdAt,
    };
    this.proposals.set(record.id, record);
    this.proposalOrder.push(record.id);
    return { ...record };
  }

  async get(proposalId: string): Promise<ProposalRecord | null> {
    const proposal = this.proposals.get(proposalId);
    return proposal ? { ...proposal } : null;
  }

  async getByActionId(
    scopeKey: string,
    actionId: string,
  ): Promise<ProposalRecord | null> {
    // Most recent wins: walk insertion order backwards for the latest match —
    // a released key (rejected/invalidated) can be re-used, so recency, not
    // uniqueness, decides which record answers this query.
    for (let i = this.proposalOrder.length - 1; i >= 0; i--) {
      const proposal = this.proposals.get(this.proposalOrder[i]!);
      if (
        proposal &&
        proposal.scopeKey === scopeKey &&
        proposal.actionId === actionId
      ) {
        return { ...proposal };
      }
    }
    return null;
  }

  async listByChat(
    chatId: string,
    opts?: ListProposalsOptions,
  ): Promise<ProposalRecord[]> {
    const rows = [...this.proposals.values()].filter(
      (p) =>
        p.chatId === chatId &&
        (opts?.status === undefined || p.status === opts.status),
    );
    const limited =
      opts?.limit !== undefined ? rows.slice(0, opts.limit) : rows;
    return limited.map((p) => ({ ...p }));
  }

  async listByStatus(
    status: ProposalStatus,
    opts?: { limit?: number },
  ): Promise<ProposalRecord[]> {
    const rows = [...this.proposals.values()].filter(
      (p) => p.status === status,
    );
    const limited =
      opts?.limit !== undefined ? rows.slice(0, opts.limit) : rows;
    return limited.map((p) => ({ ...p }));
  }

  async transition(
    proposalId: string,
    from: ProposalStatus[],
    to: ProposalStatus,
    patch?: ProposalPatch,
  ): Promise<ProposalRecord> {
    const proposal = this.proposals.get(proposalId);
    if (!proposal) {
      throw new RecordNotFoundError(`Proposal not found: ${proposalId}`);
    }
    if (!from.includes(proposal.status)) {
      throw new InvalidProposalTransitionError(
        `Proposal ${proposalId} is ${proposal.status}, expected one of [${from.join(", ")}].`,
        { proposalId, current: proposal.status, from, to },
      );
    }
    assertProposalTransition(proposal.status, to);
    proposal.status = to;
    if (patch?.decision !== undefined) proposal.decision = patch.decision;
    if (patch?.decidedAt !== undefined) proposal.decidedAt = patch.decidedAt;
    if (patch?.appliedAt !== undefined) proposal.appliedAt = patch.appliedAt;
    if (patch?.operationId !== undefined) {
      proposal.operationId = patch.operationId;
    }
    if (patch?.reason !== undefined) proposal.reason = patch.reason;
    return { ...proposal };
  }

  async recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome> {
    // Idempotent on operationId: the first outcome for an operation is the
    // one that happened, and later calls must not rewrite the evidence.
    const existing = this.outcomes.get(operationId);
    if (existing) return { ...existing, failedOps: [...existing.failedOps] };
    // Store (and return) a copy — never the caller's own object — so a
    // caller that mutates the outcome it just built cannot corrupt the
    // recorded evidence after the fact.
    const stored: ApplyOutcome = {
      ...outcome,
      failedOps: [...outcome.failedOps],
    };
    this.outcomes.set(operationId, stored);
    return { ...stored, failedOps: [...stored.failedOps] };
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    const outcome = this.outcomes.get(operationId);
    return outcome ? { ...outcome, failedOps: [...outcome.failedOps] } : null;
  }

  async invalidatePendingForRevision(
    scopeKey: string,
    newRevision: string,
  ): Promise<number> {
    let count = 0;
    for (const proposal of this.proposals.values()) {
      if (proposal.scopeKey !== scopeKey) continue;
      if (proposal.status !== "pending") continue;
      if (proposal.revisionAtCreate === newRevision) continue;
      assertProposalTransition(proposal.status, "invalidated");
      proposal.status = "invalidated";
      proposal.reason = "revision_conflict";
      proposal.decidedAt = this.clock.nowIso();
      count++;
    }
    return count;
  }

  /**
   * Drop a chat's proposals and the outcomes they claimed — by `chatId`, never
   * by `scopeKey` (see the port: two chats can propose into one scope).
   */
  async deleteByChat(chatId: string): Promise<number> {
    const doomed = [...this.proposals.values()].filter(
      (proposal) => proposal.chatId === chatId,
    );
    for (const proposal of doomed) {
      if (proposal.operationId !== undefined) {
        this.outcomes.delete(proposal.operationId);
      }
      this.proposals.delete(proposal.id);
      const at = this.proposalOrder.indexOf(proposal.id);
      if (at !== -1) this.proposalOrder.splice(at, 1);
    }
    return doomed.length;
  }
}

export class MemoryProviderStore implements ProviderStore {
  readonly providers = new Map<string, AiProviderConfig>();
  readonly models = new Map<string, AiProviderModel[]>();
  readonly capabilities = new Map<string, AiProviderCapabilities>();

  async listProviders(): Promise<AiProviderConfig[]> {
    return [...this.providers.values()].map((p) => ({ ...p }));
  }
  async getProvider(providerId: string): Promise<AiProviderConfig | null> {
    const config = this.providers.get(providerId);
    return config ? { ...config } : null;
  }
  async upsertProvider(config: AiProviderConfig): Promise<AiProviderConfig> {
    // Store (and return) a copy — never the caller's own object.
    const stored = { ...config };
    this.providers.set(config.id, stored);
    return { ...stored };
  }
  async deleteProvider(providerId: string): Promise<void> {
    this.providers.delete(providerId);
    this.models.delete(providerId);
    this.capabilities.delete(providerId);
  }
  async listModels(providerId: string): Promise<AiProviderModel[]> {
    return (this.models.get(providerId) ?? []).map((m) => ({ ...m }));
  }
  async replaceModels(
    providerId: string,
    models: AiProviderModel[],
  ): Promise<void> {
    this.models.set(
      providerId,
      models.map((m) => ({ ...m })),
    );
  }
  async getCapabilities(
    providerId: string,
  ): Promise<AiProviderCapabilities | null> {
    const capabilities = this.capabilities.get(providerId);
    return capabilities ? { ...capabilities } : null;
  }
  async saveCapabilities(
    providerId: string,
    capabilities: AiProviderCapabilities,
  ): Promise<void> {
    this.capabilities.set(providerId, { ...capabilities });
  }
}

export class MemorySettingsStore implements SettingsStore {
  private settings: AssistantSettings = {
    contextSizePreference: "small",
    writePolicyMode: "auto_readonly_confirm_writes",
    allowRawToolData: false,
    metadata: {},
  };

  async getSettings(): Promise<AssistantSettings> {
    return { ...this.settings };
  }

  async updateSettings(
    patch: Partial<AssistantSettings>,
  ): Promise<AssistantSettings> {
    this.settings = { ...this.settings, ...patch };
    return { ...this.settings };
  }
}

export class MemoryOutboxStore implements OutboxStore {
  private readonly records = new Map<string, OutboxRecord>();
  private readonly order: string[] = [];

  constructor(
    private readonly clock: Clock,
    private readonly claimVisibilityMs: number = DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS,
  ) {}

  async enqueue(input: OutboxAppendInput): Promise<OutboxRecord> {
    const now = this.clock.nowIso();
    const record: OutboxRecord = {
      id: input.id ?? `outbox_${crypto.randomUUID()}`,
      topic: input.topic,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      payload: input.payload,
      createdAt: now,
      availableAt: input.availableAt ?? now,
      attempts: 0,
    };
    this.records.set(record.id, record);
    this.order.push(record.id);
    return { ...record };
  }

  async claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]> {
    const nowMs = input.now.getTime();
    const claimed: OutboxRecord[] = [];
    for (const id of this.order) {
      if (claimed.length >= input.limit) break;
      const record = this.records.get(id);
      if (!record) continue;
      if (record.publishedAt !== undefined) continue;
      if (new Date(record.availableAt).getTime() > nowMs) continue;
      record.attempts += 1;
      // Push the visibility window forward so a concurrent claimBatch call
      // does not hand the same in-flight record to a second publisher before
      // markPublished/markFailed resolves it. The port has no separate
      // "claimed" flag — availableAt doubles as the claim lease, the same
      // trick a visibility-timeout queue uses.
      record.availableAt = new Date(
        nowMs + this.claimVisibilityMs,
      ).toISOString();
      claimed.push({ ...record });
    }
    return claimed;
  }

  async markPublished(id: string, at: Date): Promise<void> {
    const record = this.records.get(id);
    if (record) record.publishedAt = at.toISOString();
  }

  async markFailed(id: string, error: string, retryAt: Date): Promise<void> {
    const record = this.records.get(id);
    if (record) {
      record.lastError = error;
      record.availableAt = retryAt.toISOString();
    }
  }
}

/**
 * Map-backed, complete {@link AssistantStore}.
 *
 * NO ROLLBACK: `transaction(fn)` simply runs `fn(this)` — every write inside
 * it lands on the live Maps immediately, and a throw after some writes leaves
 * those writes in place. That is fine for tests and single-process embedding
 * where "transaction" mostly means "these calls are logically one unit", but
 * it is NOT what a host relying on atomicity for crash-consistency should
 * reach for — see {@link SqliteAssistantStore} for a store with a real
 * BEGIN/COMMIT/ROLLBACK. `capabilities.atomicTransactions` in the
 * conformance suite's factory exists specifically so the atomicity test skips
 * this adapter instead of failing it.
 */
export class MemoryAssistantStore implements AssistantStore {
  readonly conversations: MemoryConversationStore;
  readonly tasks: MemoryTaskStore;
  readonly proposals: MemoryProposalStore;
  readonly providers = new MemoryProviderStore();
  readonly settings = new MemorySettingsStore();
  readonly outbox: MemoryOutboxStore;

  constructor(options: MemoryAssistantStoreOptions = {}) {
    const clock = options.clock ?? defaultClock;
    const ids = options.ids ?? defaultIds;
    this.conversations = new MemoryConversationStore(clock, ids);
    this.tasks = new MemoryTaskStore(clock, ids, options.leaseTtlMs, options);
    this.proposals = new MemoryProposalStore(clock);
    this.outbox = new MemoryOutboxStore(clock, options.outboxClaimVisibilityMs);
  }

  async transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> {
    return fn(this);
  }
}
