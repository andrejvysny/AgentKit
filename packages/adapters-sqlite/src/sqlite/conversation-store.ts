/**
 * `bun:sqlite`-backed {@link ConversationStore}: chats, messages, the branch
 * tree (`active`/`depth`/`branch_index`), and FTS5 search over
 * `message_search`.
 *
 * Split out of `sqlite-assistant-store.ts` — one sub-store per file, sharing
 * {@link SqliteConnection} and the row mappers in `rows.js`.
 */
import {
  activationSetOf,
  activePathOf,
  assertAppendActivation,
  assertListMessagesCursors,
  DEFAULT_SEARCH_LIMIT,
  forkedChatTitle,
  forkPrefixOf,
  InvalidImportError,
  planForkedMessages,
  planImportedMessages,
  RecordNotFoundError,
  SEARCH_MATCH_END,
  SEARCH_MATCH_START,
  SEARCH_SNIPPET_ELLIPSIS,
  type AppendMessageInput,
  type ChatRecord,
  type Clock,
  type ConversationStore,
  type CreateChatInput,
  type ForkChatResult,
  type IdGenerator,
  type ImportConversationInput,
  type ListChatsOptions,
  type ListMessagesOptions,
  type MessageRecord,
  type MessageSearchHit,
  type SearchMessagesOptions,
  type UpdateChatPatch,
  type UpdateMessagePatch,
} from "@agentkit/host";
import type { Params, SqliteConnection, TxOwner } from "./connection.js";
import {
  type ChatRow,
  chatFromRow,
  encodeContent,
  type MessageRow,
  messageFromRow,
  toIntBool,
  toJson,
} from "./rows.js";

/** How many tokens of context `snippet()` builds a window from. */
const SNIPPET_TOKENS = 16;

/**
 * FTS5's query language is a hazard, not a feature, when the input is a search
 * box: `*` is a prefix operator, `^` anchors a column, `"` opens a phrase, `-`
 * negates, `:` filters a column, `AND`/`OR`/`NOT`/`NEAR` are keywords, and an
 * unbalanced any-of-them is a raw SQLite error out of a method that documents
 * none. A user typing `c++ (2)` is not writing a query; they are typing what
 * they remember seeing.
 *
 * So the raw string is never handed to FTS5. The operator characters that
 * cannot survive quoting are removed (`"` first, so nothing downstream can
 * break out of the quotes this function adds), parentheses become spaces so
 * `(2)` still searches for `2`, and every remaining whitespace-separated token
 * is re-emitted as a QUOTED PHRASE. Quoting is what neutralises the rest:
 * inside `"..."` a hyphen, a colon and the word `AND` are all just text.
 * Several tokens juxtaposed are FTS5's implicit AND, so `quartz beacon` means
 * "both words", which is what a person typing two words means.
 *
 * `null` when nothing survives — the port's "empty after sanitizing returns no
 * hits", expressed as a value the caller cannot forget to check.
 */
function toFtsQuery(query: string): string | null {
  const tokens = query
    .replace(/["*^]/g, "")
    .replace(/[()]/g, " ")
    .split(/\s+/)
    .filter((token) => token.length > 0);
  if (tokens.length === 0) return null;
  return tokens.map((token) => `"${token}"`).join(" ");
}

export class SqliteConversationStore implements ConversationStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async createChat(input: CreateChatInput): Promise<ChatRecord> {
    const now = this.clock.nowIso();
    const id = input.id ?? this.ids.chatId();
    const metadata = input.metadata ?? {};
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO chats (id, title, created_at, updated_at, metadata)
         VALUES ($id, $title, $now, $now, $metadata)`,
        {
          $id: id,
          $title: input.title ?? null,
          $now: now,
          $metadata: toJson(metadata),
        },
      );
    }, this.txOwner);
    return {
      id,
      ...(input.title === undefined ? {} : { title: input.title }),
      createdAt: now,
      updatedAt: now,
      metadata,
      archived: false,
    };
  }

  async getChat(chatId: string): Promise<ChatRecord | null> {
    const row = this.conn.get(`SELECT * FROM chats WHERE id = $id`, {
      $id: chatId,
    }) as ChatRow | null;
    return row ? chatFromRow(row) : null;
  }

  async listChats(opts?: ListChatsOptions): Promise<ChatRecord[]> {
    const where: string[] = [];
    const params: Params = {};
    if (opts?.ids === undefined) {
      // The browse: archived chats are out unless asked for.
      if (opts?.includeArchived !== true) where.push(`archived = 0`);
    } else {
      // The batch fetch. An explicit id resolves an archived chat too, and an
      // EMPTY list resolves nothing — inlined as a bound placeholder per id,
      // because bun:sqlite's named parameters cannot bind an array.
      if (opts.ids.length === 0) return [];
      const names = opts.ids.map((id, index) => {
        params[`$id${index}`] = id;
        return `$id${index}`;
      });
      where.push(`id IN (${names.join(", ")})`);
    }
    if (opts?.before !== undefined) {
      where.push(`updated_at < $before`);
      params.$before = opts.before;
    }
    let sql = `SELECT * FROM chats`;
    if (where.length > 0) sql += ` WHERE ${where.join(" AND ")}`;
    sql += ` ORDER BY updated_at DESC`;
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as ChatRow[];
    return rows.map(chatFromRow);
  }

  /**
   * Patch a chat and answer with the row as it now stands.
   *
   * COALESCE over three columns rather than a built-up SET list: every field is
   * nullable-in-the-patch and not-null-in-the-row, so "leave it alone" is
   * expressible as a bound NULL and the statement is the same one every time.
   * The read-back is inside the transaction, so what comes back is what this
   * write produced rather than what a concurrent append made of it.
   */
  async updateChat(
    chatId: string,
    patch: UpdateChatPatch,
  ): Promise<ChatRecord> {
    return this.conn.whenFree(() => {
      const changes = this.conn.run(
        `UPDATE chats SET
           title = COALESCE($title, title),
           metadata = COALESCE($metadata, metadata),
           archived = COALESCE($archived, archived),
           updated_at = $now
         WHERE id = $id`,
        {
          $title: patch.title ?? null,
          // Metadata REPLACES the stored bag, per the port contract.
          $metadata:
            patch.metadata === undefined ? null : toJson(patch.metadata),
          $archived:
            patch.archived === undefined ? null : toIntBool(patch.archived),
          $now: this.clock.nowIso(),
          $id: chatId,
        },
      );
      if (changes.changes === 0) {
        throw new RecordNotFoundError(`Chat not found: ${chatId}`);
      }
      const row = this.conn.get(`SELECT * FROM chats WHERE id = $id`, {
        $id: chatId,
      }) as ChatRow;
      return chatFromRow(row);
    }, this.txOwner);
  }

  /**
   * Delete the chat and every message in it, in ONE transaction.
   *
   * The messages go in a single statement even though `parent_message_id` is a
   * self-FK and the delete necessarily removes parents alongside their
   * children: SQLite checks an IMMEDIATE foreign key at the END of the
   * statement, not per row, so a set that is closed under the relation — which
   * every message of one chat is, since a parent is always in the same chat —
   * leaves nothing in violation to report.
   *
   * The FTS index needs no separate cleanup: `messages_search_delete` fires per
   * deleted row inside this same transaction, so a rolled-back delete un-deletes
   * the index too.
   */
  async deleteChat(chatId: string): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(`DELETE FROM messages WHERE chat_id = $chatId`, {
        $chatId: chatId,
      });
      const changes = this.conn.run(`DELETE FROM chats WHERE id = $id`, {
        $id: chatId,
      });
      if (changes.changes === 0) {
        throw new RecordNotFoundError(`Chat not found: ${chatId}`);
      }
    }, this.txOwner);
  }

  /**
   * Append a message and place it in the chat's tree, in ONE transaction.
   *
   * Everything the placement needs is read with a targeted, indexed query rather
   * than by loading the chat: the active leaf, the sibling high-water mark, the
   * parent row. The whole tree is only materialized when a path switch actually
   * has to be computed, which on the append-to-the-active-leaf path — every
   * append a non-branching caller makes — is never.
   */
  async appendMessage(input: AppendMessageInput): Promise<MessageRecord> {
    assertAppendActivation(input);
    return this.conn.whenFree(() => {
      const chat = this.conn.get(`SELECT id FROM chats WHERE id = $id`, {
        $id: input.chatId,
      });
      if (!chat) {
        throw new RecordNotFoundError(`Chat not found: ${input.chatId}`);
      }
      const leaf = this.activeLeafRow(input.chatId);
      // An explicitly named parent is a structural claim and is checked as one;
      // an absent one means "continue the conversation", which is the leaf.
      const parent =
        input.parentMessageId === undefined
          ? leaf
          : this.requireParentRow(input.chatId, input.parentMessageId);
      const parentId = parent === null ? null : parent.id;
      // A CHAIN append moves no path and takes its `active` from the parent
      // instead. `assertAppendActivation` has already proved a parent was
      // named, so `parent` is non-null here whenever this is true.
      const chained = input.activate === false;
      const maxRow = this.conn.get(
        `SELECT COALESCE(MAX(order_key), 0) as maxKey FROM messages WHERE chat_id = $chatId`,
        { $chatId: input.chatId },
      ) as { maxKey: number };
      const orderKey = maxRow.maxKey + 1;
      // Both facts about this parent's children in one indexed pass over
      // `idx_messages_parent`: the next branch index, and whether one of them
      // is already on the live path. The second is what a CHAIN append needs —
      // it may inherit `active: true` only from a parent that is still the END
      // of the live chain (see `hasActiveChild` and the port's `activate`), or
      // one message would end up with two active children.
      const branchRow = this.conn.get(
        `SELECT COALESCE(MAX(branch_index) + 1, 0) as nextIndex,
                COALESCE(MAX(active), 0) as hasActiveChild
         FROM messages
         WHERE chat_id = $chatId AND parent_message_id IS $parentId`,
        { $chatId: input.chatId, $parentId: parentId },
      ) as { nextIndex: number; hasActiveChild: number };
      const active = chained
        ? parent !== null && parent.active && branchRow.hasActiveChild === 0
        : true;
      const id = input.id ?? this.ids.messageId();
      const now = this.clock.nowIso();
      const metadata = input.metadata ?? {};
      const depth = parent === null ? 0 : parent.depth + 1;
      const encoded = encodeContent(input.content);
      this.conn.run(
        `INSERT INTO messages
           (id, chat_id, run_id, role, content, content_format, order_key, tool_call_id, tool_calls, model_result_json, parent_message_id, depth, branch_index, active, metadata, created_at)
         VALUES
           ($id, $chatId, $runId, $role, $content, $contentFormat, $orderKey, $toolCallId, $toolCalls, $modelResultJson, $parentId, $depth, $branchIndex, $active, $metadata, $now)`,
        {
          $id: id,
          $chatId: input.chatId,
          $runId: input.runId ?? null,
          $role: input.role,
          $content: encoded.content,
          $contentFormat: encoded.format,
          $orderKey: orderKey,
          $toolCallId: input.toolCallId ?? null,
          $toolCalls:
            input.toolCalls === undefined ? null : toJson(input.toolCalls),
          $modelResultJson: input.modelResultJson ?? null,
          $parentId: parentId,
          $depth: depth,
          $branchIndex: branchRow.nextIndex,
          $active: toIntBool(active),
          $metadata: toJson(metadata),
          $now: now,
        },
      );
      // Only a branch — an append whose parent is NOT the message the
      // conversation currently ends at — moves the path. Hanging a new leaf off
      // the old leaf leaves every other flag exactly as it was, and running the
      // switch anyway would mean reading the whole chat on every streamed tool
      // result to write the flags back unchanged.
      const parentIsActiveLeaf = (leaf === null ? null : leaf.id) === parentId;
      if (!chained && !parentIsActiveLeaf) {
        this.switchActivePath(input.chatId, id);
      }
      this.conn.run(`UPDATE chats SET updated_at = $now WHERE id = $chatId`, {
        $now: now,
        $chatId: input.chatId,
      });
      return {
        id,
        chatId: input.chatId,
        ...(input.runId === undefined ? {} : { runId: input.runId }),
        role: input.role,
        content: input.content,
        orderKey,
        ...(input.toolCallId === undefined
          ? {}
          : { toolCallId: input.toolCallId }),
        ...(input.toolCalls === undefined
          ? {}
          : { toolCalls: input.toolCalls }),
        ...(input.modelResultJson === undefined
          ? {}
          : { modelResultJson: input.modelResultJson }),
        ...(parentId === null ? {} : { parentMessageId: parentId }),
        depth,
        branchIndex: branchRow.nextIndex,
        active,
        metadata,
        createdAt: now,
      };
    }, this.txOwner);
  }

  /** The chat's deepest active message, or null when nothing is active. */
  private activeLeafRow(chatId: string): MessageRecord | null {
    const row = this.conn.get(
      `SELECT * FROM messages WHERE chat_id = $chatId AND active = 1
       ORDER BY depth DESC, order_key DESC LIMIT 1`,
      { $chatId: chatId },
    ) as MessageRow | null;
    return row === null ? null : messageFromRow(row);
  }

  /** The named parent, proven to exist and to be in the same chat. */
  private requireParentRow(
    chatId: string,
    parentMessageId: string,
  ): MessageRecord {
    const row = this.conn.get(
      `SELECT * FROM messages WHERE id = $id AND chat_id = $chatId`,
      { $id: parentMessageId, $chatId: chatId },
    ) as MessageRow | null;
    if (row === null) {
      throw new RecordNotFoundError(
        `Parent message not found in chat ${chatId}: ${parentMessageId}`,
      );
    }
    return messageFromRow(row);
  }

  /** Every message in one chat — what the tree walks need and the queries cannot. */
  private chatMessages(chatId: string): MessageRecord[] {
    const rows = this.conn.all(
      `SELECT * FROM messages WHERE chat_id = $chatId`,
      { $chatId: chatId },
    ) as MessageRow[];
    return rows.map(messageFromRow);
  }

  /**
   * Rewrite the chat's `active` flags so `messageId`'s path is the live one.
   *
   * A DIFF, not a blanket clear-then-set: the rows whose flag already agrees are
   * left alone, so switching between two branches of a long conversation writes
   * the handful of rows that actually changed instead of every message in the
   * chat. Caller must already hold the transaction.
   *
   * Returns the path it just made live. The loaded records are re-flagged in
   * memory as they are written, so the answer is `activePathOf` over the state
   * this call produced — the same sort `listMessages` applies, without a second
   * query and without leaving the transaction to ask.
   */
  private switchActivePath(chatId: string, messageId: string): MessageRecord[] {
    const records = this.chatMessages(chatId);
    const active = activationSetOf(records, messageId);
    for (const record of records) {
      const next = active.has(record.id);
      if (next === record.active) continue;
      this.conn.run(`UPDATE messages SET active = $active WHERE id = $id`, {
        $active: toIntBool(next),
        $id: record.id,
      });
      record.active = next;
    }
    return activePathOf(records);
  }

  async updateMessage(
    messageId: string,
    patch: UpdateMessagePatch,
  ): Promise<MessageRecord> {
    return this.conn.whenFree(() => {
      const existing = this.conn.get(`SELECT * FROM messages WHERE id = $id`, {
        $id: messageId,
      }) as MessageRow | null;
      if (!existing) {
        throw new RecordNotFoundError(`Message not found: ${messageId}`);
      }
      // Re-encoded when the patch carries a body, carried through as the stored
      // columns when it does not. Both halves move together or not at all: a
      // string patch landing on a `'parts'` row that kept its old format tag
      // would read back as JSON that is not JSON.
      const encoded =
        patch.content === undefined
          ? { content: existing.content, format: existing.content_format }
          : encodeContent(patch.content);
      // Metadata REPLACES the stored bag, per the port contract.
      const metadataJson =
        patch.metadata !== undefined
          ? toJson(patch.metadata)
          : existing.metadata;
      const toolCallsJson =
        patch.toolCalls !== undefined
          ? toJson(patch.toolCalls)
          : existing.tool_calls;
      this.conn.run(
        `UPDATE messages SET content = $content, content_format = $contentFormat, metadata = $metadata, tool_calls = $toolCalls WHERE id = $id`,
        {
          $content: encoded.content,
          $contentFormat: encoded.format,
          $metadata: metadataJson,
          $toolCalls: toolCallsJson,
          $id: messageId,
        },
      );
      return messageFromRow({
        ...existing,
        content: encoded.content,
        content_format: encoded.format,
        metadata: metadataJson,
        tool_calls: toolCallsJson,
      });
    }, this.txOwner);
  }

  /** The chat's ACTIVE PATH, `(depth, orderKey)` ascending — see the port. */
  async listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]> {
    assertListMessagesCursors(opts);
    let sql = `SELECT * FROM messages WHERE chat_id = $chatId AND active = 1`;
    const params: Params = { $chatId: chatId };
    if (opts?.afterOrderKey !== undefined) {
      sql += ` AND order_key > $after`;
      params.$after = opts.afterOrderKey;
    }
    if (opts?.beforeOrderKey !== undefined) {
      sql += ` AND order_key < $before`;
      params.$before = opts.beforeOrderKey;
    }
    sql += ` ORDER BY depth ASC, order_key ASC`;
    const rows = this.conn.all(sql, params) as MessageRow[];
    const mapped = rows.map(messageFromRow);
    // The LAST `limit` in either direction: a scroll-back wants the page
    // nearest its cursor, exactly as a plain listing wants the newest page.
    return opts?.limit !== undefined ? mapped.slice(-opts.limit) : mapped;
  }

  /**
   * The deepest record a run wrote in this chat — see the port.
   *
   * `(depth, order_key)` descending with `LIMIT 1`, and deliberately NO
   * `active` filter: a run whose branch was abandoned mid-turn still has to
   * continue its own chain, and a lookup that only saw the live path would hand
   * it a link into somebody else's conversation.
   */
  async lastMessageOfRun(
    chatId: string,
    runId: string,
  ): Promise<MessageRecord | null> {
    const row = this.conn.get(
      `SELECT * FROM messages
        WHERE chat_id = $chatId AND run_id = $runId
        ORDER BY depth DESC, order_key DESC
        LIMIT 1`,
      { $chatId: chatId, $runId: runId },
      // `conn.get` answers `null` (not `undefined`) when nothing matched.
    ) as MessageRow | null;
    return row === null ? null : messageFromRow(row);
  }

  /**
   * The message's siblings, itself included, `branchIndex` ascending.
   *
   * Answered in SQL off `idx_messages_parent` rather than by walking a loaded
   * tree; the ORDER BY is the same order `siblingsOf` produces, and the
   * conformance suite grades both adapters on it so the two cannot drift.
   * `IS $parentId` rather than `= $parentId` because a root's parent is NULL,
   * which `=` never matches.
   */
  async listSiblings(messageId: string): Promise<MessageRecord[]> {
    return this.conn.withTx(() => {
      const record = this.requireMessage(messageId);
      const rows = this.conn.all(
        `SELECT * FROM messages
         WHERE chat_id = $chatId AND parent_message_id IS $parentId
         ORDER BY branch_index ASC, order_key ASC`,
        {
          $chatId: record.chatId,
          $parentId: record.parentMessageId ?? null,
        },
      ) as MessageRow[];
      return rows.map(messageFromRow);
    });
  }

  async activatePath(messageId: string): Promise<MessageRecord[]> {
    return this.conn.whenFree(() => {
      const record = this.requireMessage(messageId);
      return this.switchActivePath(record.chatId, messageId);
    }, this.txOwner);
  }

  /**
   * Copy the source chat's active-path prefix into a new chat, in ONE
   * transaction — the chat row and every message, or neither.
   *
   * `withTx` (synchronous) rather than the store's async `transaction`: this is
   * one port method's own SQL, there is nothing to await inside it, and an
   * `await` between the BEGIN and the COMMIT is precisely what a bun:sqlite
   * transaction cannot survive. Called from inside a host-level
   * `store.transaction(...)` it flattens into that one instead, so a fork that
   * an outer transaction later rolls back leaves nothing behind.
   *
   * Messages are inserted ROOT FIRST, which the self-FK on `parent_message_id`
   * requires: a child inserted before its parent would be rejected by the
   * constraint rather than by a later read.
   */
  async forkChat(
    chatId: string,
    fromMessageId: string,
  ): Promise<ForkChatResult> {
    return this.conn.whenFree(() => {
      const sourceRow = this.conn.get(`SELECT * FROM chats WHERE id = $id`, {
        $id: chatId,
      }) as ChatRow | null;
      if (sourceRow === null) {
        throw new RecordNotFoundError(`Chat not found: ${chatId}`);
      }
      const source = chatFromRow(sourceRow);
      const prefix = forkPrefixOf(
        this.chatMessages(chatId),
        chatId,
        fromMessageId,
      );
      const plans = planForkedMessages(prefix, () => this.ids.messageId());
      const now = this.clock.nowIso();
      const title = forkedChatTitle(source.title);
      // The chat's own metadata IS copied (its labels, its owner, whatever the
      // host keeps there); only per-MESSAGE run linkage is stripped, and a fork
      // that lost the conversation's own bookkeeping would be a different chat
      // rather than a copy of this one.
      const metadata = { ...source.metadata };
      const forkId = this.ids.chatId();
      this.conn.run(
        `INSERT INTO chats (id, title, created_at, updated_at, metadata)
         VALUES ($id, $title, $now, $now, $metadata)`,
        {
          $id: forkId,
          $title: title ?? null,
          $now: now,
          $metadata: toJson(metadata),
        },
      );

      const messages: MessageRecord[] = [];
      for (const plan of plans) {
        const orderKey = messages.length + 1;
        const encoded = encodeContent(plan.source.content);
        this.conn.run(
          `INSERT INTO messages
             (id, chat_id, run_id, role, content, content_format, order_key, tool_call_id, tool_calls, model_result_json, parent_message_id, depth, branch_index, active, metadata, created_at)
           VALUES
             ($id, $chatId, NULL, $role, $content, $contentFormat, $orderKey, $toolCallId, $toolCalls, $modelResultJson, $parentId, $depth, 0, 1, $metadata, $now)`,
          {
            $id: plan.id,
            $chatId: forkId,
            $role: plan.source.role,
            $content: encoded.content,
            $contentFormat: encoded.format,
            $orderKey: orderKey,
            $toolCallId: plan.source.toolCallId ?? null,
            $toolCalls:
              plan.source.toolCalls === undefined
                ? null
                : toJson(plan.source.toolCalls),
            $modelResultJson: plan.source.modelResultJson ?? null,
            $parentId: plan.parentMessageId ?? null,
            $depth: plan.depth,
            $metadata: toJson(plan.metadata),
            $now: now,
          },
        );
        messages.push({
          id: plan.id,
          chatId: forkId,
          role: plan.source.role,
          content: plan.source.content,
          orderKey,
          ...(plan.source.toolCallId === undefined
            ? {}
            : { toolCallId: plan.source.toolCallId }),
          ...(plan.source.toolCalls === undefined
            ? {}
            : { toolCalls: plan.source.toolCalls }),
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
        });
      }

      return {
        chat: {
          id: forkId,
          ...(title === undefined ? {} : { title }),
          createdAt: now,
          updatedAt: now,
          metadata,
          // NOT inherited: a fork is a conversation somebody just started, and
          // starting one already filed away is not a state a user can have
          // meant. The INSERT above lets the column default do this.
          archived: false,
        },
        messages,
      };
    }, this.txOwner);
  }

  /**
   * Write a whole conversation with the caller's ids, in ONE transaction.
   *
   * `withTx` (synchronous), for the same reason `forkChat` uses it: this is one
   * port method's own SQL with nothing to await inside it, and an `await`
   * between the BEGIN and the COMMIT is what a bun:sqlite transaction cannot
   * survive. Inside a host-level `store.transaction(...)` it flattens into that
   * one instead.
   *
   * `planImportedMessages` has already proved the payload legal and assigned
   * every derived field, so this loop only writes. Messages go in the order
   * given, which is creation order, which the self-FK on `parent_message_id`
   * requires: a parent always precedes its children in a payload this store
   * accepted.
   */
  async importConversation(
    input: ImportConversationInput,
  ): Promise<ChatRecord> {
    return this.conn.whenFree(() => {
      const existing = this.conn.get(`SELECT id FROM chats WHERE id = $id`, {
        $id: input.chat.id,
      });
      if (existing) {
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
      const metadata = input.chat.metadata ?? {};
      const archived = input.chat.archived ?? false;
      this.conn.run(
        `INSERT INTO chats (id, title, created_at, updated_at, metadata, archived)
         VALUES ($id, $title, $createdAt, $updatedAt, $metadata, $archived)`,
        {
          $id: input.chat.id,
          $title: input.chat.title ?? null,
          $createdAt: createdAt,
          // `createdAt`, not now: an import of a year-old conversation that
          // jumped to the top of the chat list would reorder the very history
          // it was meant to preserve.
          $updatedAt: createdAt,
          $metadata: toJson(metadata),
          $archived: toIntBool(archived),
        },
      );
      for (const plan of plans) {
        const encoded = encodeContent(plan.input.content);
        this.conn.run(
          `INSERT INTO messages
             (id, chat_id, run_id, role, content, content_format, order_key, tool_call_id, tool_calls, model_result_json, parent_message_id, depth, branch_index, active, metadata, created_at)
           VALUES
             ($id, $chatId, NULL, $role, $content, $contentFormat, $orderKey, $toolCallId, $toolCalls, $modelResultJson, $parentId, $depth, $branchIndex, $active, $metadata, $createdAt)`,
          {
            $id: plan.input.id,
            $chatId: input.chat.id,
            $role: plan.input.role,
            $content: encoded.content,
            $contentFormat: encoded.format,
            $orderKey: plan.orderKey,
            // Tool linkage, verbatim — the import's only way to preserve which
            // assistant turn a tool result answers, and so the only way a
            // migrated conversation can be put back into provider order.
            $toolCallId: plan.input.toolCallId ?? null,
            $toolCalls:
              plan.input.toolCalls === undefined
                ? null
                : toJson(plan.input.toolCalls),
            $modelResultJson: plan.input.modelResultJson ?? null,
            $parentId: plan.parentMessageId ?? null,
            $depth: plan.depth,
            $branchIndex: plan.branchIndex,
            $active: toIntBool(plan.input.active),
            $metadata: toJson(plan.metadata),
            $createdAt: plan.createdAt,
          },
        );
      }
      return {
        id: input.chat.id,
        ...(input.chat.title === undefined ? {} : { title: input.chat.title }),
        createdAt,
        updatedAt: createdAt,
        metadata,
        archived,
      };
    }, this.txOwner);
  }

  /**
   * FTS5 search over `message_search`, ranked by bm25, snippet from the index.
   *
   * The filters live in the JOIN back to `messages` rather than in the index,
   * and that placement is deliberate: `internal` and `placeholder` are
   * `metadata` keys, and metadata is REWRITTEN after the fact — a placeholder
   * becomes a real answer the moment its run finishes. An index that had
   * decided at insert time would keep every finished answer permanently
   * unfindable.
   *
   * `LIMIT` is applied by SQL over the ranked set, so a chat filter that
   * excludes most hits still returns a full page rather than whatever survived
   * the first N.
   */
  async searchMessages(
    query: string,
    opts?: SearchMessagesOptions,
  ): Promise<MessageSearchHit[]> {
    const match = toFtsQuery(query);
    if (match === null) return [];
    const rows = this.conn.all(
      `SELECT m.id AS message_id, m.chat_id AS chat_id,
              snippet(message_search, 0, $markStart, $markEnd, $ellipsis, $tokens) AS snippet
         FROM message_search
         JOIN messages AS m ON m.rowid = message_search.rowid
        WHERE message_search MATCH $match
          AND ($chatId IS NULL OR m.chat_id = $chatId)
          AND COALESCE(json_extract(m.metadata, '$.internal'), 0) <> 1
          AND COALESCE(json_extract(m.metadata, '$.placeholder'), 0) <> 1
        ORDER BY bm25(message_search)
        LIMIT $limit`,
      {
        $match: match,
        $chatId: opts?.chatId ?? null,
        $markStart: SEARCH_MATCH_START,
        $markEnd: SEARCH_MATCH_END,
        $ellipsis: SEARCH_SNIPPET_ELLIPSIS,
        $tokens: SNIPPET_TOKENS,
        $limit: opts?.limit ?? DEFAULT_SEARCH_LIMIT,
      },
    ) as { message_id: string; chat_id: string; snippet: string }[];
    return rows.map((row) => ({
      chatId: row.chat_id,
      messageId: row.message_id,
      snippet: row.snippet,
    }));
  }

  private requireMessage(messageId: string): MessageRecord {
    const row = this.conn.get(`SELECT * FROM messages WHERE id = $id`, {
      $id: messageId,
    }) as MessageRow | null;
    if (row === null) {
      throw new RecordNotFoundError(`Message not found: ${messageId}`);
    }
    return messageFromRow(row);
  }
}
