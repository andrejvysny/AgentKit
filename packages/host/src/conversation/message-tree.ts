/**
 * The tree arithmetic behind {@link ConversationStore}'s branching methods —
 * pure, synchronous, and shared by every adapter.
 *
 * It lives here, above the adapters, for one reason: "which messages are on the
 * active path", "which child does a branch switch descend into", and "what does
 * a fork copy" are BEHAVIOUR, not storage. Two adapters that each wrote their
 * own walk would each be a slightly different conversation model, and the
 * store-conformance suite would be grading two implementations of a rule that
 * only exists once. Adapters own their queries; they do not own these answers.
 *
 * Everything here is synchronous by design. The store methods that call it run
 * inside a transaction, and a `bun:sqlite` transaction cannot survive an
 * `await` — so a helper that returned a promise would be a helper no adapter
 * could use where it matters.
 */
import type { AiMessageContent } from "@agentkit/contracts";
import {
  AgentKitHostError,
  InvalidForkPointError,
  InvalidImportError,
} from "../errors.js";
import type {
  AppendMessageInput,
  ImportMessageInput,
  ListMessagesOptions,
  MessageRecord,
} from "../ports/conversation-store.js";
import { orderMessagesForProvider } from "../turn/message-order.js";

/** `metadata` key marking an answer still streaming; never copied by a fork. */
const PLACEHOLDER_KEY = "placeholder";

/** `metadata` key marking a replay-only record the user never saw as chat. */
const INTERNAL_KEY = "internal";

/** What separates two text parts in a message's searchable projection. */
const SEARCH_TEXT_SEPARATOR = "\n";

/** Path order: down the tree first, then append order inside a depth. */
function byDepthThenOrderKey(a: MessageRecord, b: MessageRecord): number {
  return a.depth - b.depth || a.orderKey - b.orderKey;
}

/** Sibling order: declared branch order first, append order as the tiebreak. */
function byBranchIndexThenOrderKey(a: MessageRecord, b: MessageRecord): number {
  return a.branchIndex - b.branchIndex || a.orderKey - b.orderKey;
}

/**
 * The chat's active path, root first.
 *
 * A filter and a sort, not a walk — that is the whole payoff of storing the path
 * as a per-message flag. It is also why this is correct on a chat whose flags
 * were written by an older build: whatever is marked active comes back in tree
 * order, rather than a walk stopping dead at the first missing link.
 */
export function activePathOf(
  records: readonly MessageRecord[],
): MessageRecord[] {
  return records.filter((record) => record.active).sort(byDepthThenOrderKey);
}

/** The deepest active message — what an unparented append hangs itself from. */
export function activeLeafOf(
  records: readonly MessageRecord[],
): MessageRecord | undefined {
  return activePathOf(records).at(-1);
}

/** Children of a parent (or the chat's roots, for `undefined`), sibling order. */
export function childrenOf(
  records: readonly MessageRecord[],
  parentMessageId: string | undefined,
): MessageRecord[] {
  return records
    .filter((record) => record.parentMessageId === parentMessageId)
    .sort(byBranchIndexThenOrderKey);
}

/**
 * The `branchIndex` the next child of this parent takes: one past the highest
 * already used, or `0` when there are none.
 *
 * Max-plus-one rather than a count, because indices are permanent: deleting the
 * second of three answers must not hand the fourth an index the client already
 * knows as something else.
 */
export function nextBranchIndex(
  records: readonly MessageRecord[],
  parentMessageId: string | undefined,
): number {
  const siblings = childrenOf(records, parentMessageId);
  return siblings.reduce((max, s) => Math.max(max, s.branchIndex + 1), 0);
}

/**
 * Whether this parent already has a child on the active path.
 *
 * The one extra fact a CHAIN append needs (see
 * {@link AppendMessageInput.activate}): a chain may only inherit `active: true`
 * from a parent that is the END of the live chain. A parent that is active and
 * already has an active child has been continued by someone else — a bare
 * `appendMessage` from another caller landing between two of a run's writes —
 * and inheriting there would give the chat two active children of one message,
 * which is not a path.
 *
 * The sqlite adapter answers the same question in SQL, folded into the query
 * that already computes the next `branchIndex` over the same index; this is the
 * definition both are written against.
 */
export function hasActiveChild(
  records: readonly MessageRecord[],
  parentMessageId: string | undefined,
): boolean {
  return records.some(
    (record) => record.parentMessageId === parentMessageId && record.active,
  );
}

/** A message's siblings, itself included, in `branchIndex` order. */
export function siblingsOf(
  records: readonly MessageRecord[],
  record: MessageRecord,
): MessageRecord[] {
  return childrenOf(records, record.parentMessageId);
}

/**
 * Every message id that must be active for `messageId`'s branch to be the
 * chat's path: its ancestors, itself, and a descent from it to a leaf.
 *
 * The descent prefers a child that is ALREADY active and falls back to the
 * lowest `branchIndex`. The preference only has something to prefer when
 * `messageId` is ALREADY on the live path — re-activating an ancestor of where
 * the conversation is walks back down the branch it is on, rather than
 * resetting it to that branch's first answer. It is NOT a bookmark that
 * survives leaving: the caller clears every flag outside this set in the same
 * transaction, so a branch switched away from has nothing marked active left,
 * and coming back to it later descends by lowest `branchIndex` whatever was
 * open before. Anything in the chat outside the returned set is inactive
 * afterwards; the caller writes that difference in one transaction.
 *
 * A cycle in `parentMessageId` (impossible through this port, reachable only
 * through hand-edited storage) terminates the ancestor walk rather than hanging
 * it: a corrupt chat should fail to switch, not fail to return.
 */
export function activationSetOf(
  records: readonly MessageRecord[],
  messageId: string,
): Set<string> {
  const byId = new Map(records.map((record) => [record.id, record]));
  const set = new Set<string>();
  let cursor = byId.get(messageId);
  while (cursor !== undefined && !set.has(cursor.id)) {
    set.add(cursor.id);
    cursor =
      cursor.parentMessageId === undefined
        ? undefined
        : byId.get(cursor.parentMessageId);
  }
  let node = byId.get(messageId);
  while (node !== undefined) {
    const children = childrenOf(records, node.id);
    const next = children.find((child) => child.active) ?? children[0];
    if (next === undefined || set.has(next.id)) break;
    set.add(next.id);
    node = next;
  }
  return set;
}

/**
 * Reject an append that opted out of activating without saying what to chain
 * off — {@link AppendMessageInput.activate}'s one illegal combination.
 *
 * Shared rather than re-decided per adapter for the reason everything else in
 * this module is: "which appends are legal" is behaviour, and an adapter that
 * quietly accepted `activate: false` with no parent would have to invent a
 * parent to attach the record to — the active leaf, which is precisely the
 * race the flag exists to remove.
 */
export function assertAppendActivation(
  input: Pick<AppendMessageInput, "activate" | "parentMessageId">,
): void {
  if (input.activate === false && input.parentMessageId === undefined) {
    throw new AgentKitHostError(
      "invalid_append",
      "appendMessage with activate: false must name the parentMessageId to chain off.",
    );
  }
}

/**
 * The active path up to and including `fromMessageId` — the messages a fork
 * copies — with a placeholder answer dropped, in PROVIDER order.
 *
 * `records` is ONE chat's messages, so the two ways to be an illegal fork point
 * collapse to two honest reasons: `not_in_chat` (an id this chat does not have,
 * whether it exists elsewhere or nowhere — from here those are the same fact)
 * and `inactive_branch` (a message this chat has, on a path it is not currently
 * showing). Both raise {@link InvalidForkPointError}; the reason rides in
 * `details` so a client debugging "my fork 400s" can tell a stale branch from a
 * wrong id without parsing a sentence.
 */
export function forkPrefixOf(
  records: readonly MessageRecord[],
  chatId: string,
  fromMessageId: string,
): MessageRecord[] {
  if (!records.some((record) => record.id === fromMessageId)) {
    throw new InvalidForkPointError(
      `Message not found in chat ${chatId}: ${fromMessageId}`,
      { chatId, fromMessageId, reason: "not_in_chat" },
    );
  }
  const path = activePathOf(records);
  const cut = path.findIndex((record) => record.id === fromMessageId);
  if (cut === -1) {
    throw new InvalidForkPointError(
      `Message ${fromMessageId} is not on the active path of chat ${chatId}.`,
      { chatId, fromMessageId, reason: "inactive_branch" },
    );
  }
  // The placeholder filter runs AFTER the cut, not before: a fork FROM a
  // still-streaming answer is a legal request (the user forks the moment they
  // see the question they meant to ask), it just copies everything above it.
  const prefix = path
    .slice(0, cut + 1)
    .filter((record) => record.metadata[PLACEHOLDER_KEY] !== true);
  // REPAIRED BEFORE COPYING, not after. A source chat stores its turns in the
  // order they were written — the visible answer first, as an empty
  // placeholder created at submit time, then the internal assistant turn and
  // its tool results underneath it — and only `runId` tells a later replay how
  // to put those three back into the order a provider accepts. A fork strips
  // `runId` (the runs stay with the source's task log), so whatever order the
  // copies are written in is the order the fork replays FOREVER. Doing the
  // repair here is what makes the fork's stored order already BE provider
  // order, instead of leaving a chat that hands the model a tool result before
  // the turn that asked for it.
  return orderMessagesForProvider(prefix);
}

/** One copied message, with everything the source no longer gets to decide. */
export interface ForkedMessagePlan {
  /** Freshly minted; the copy shares no id with its source. */
  id: string;
  source: MessageRecord;
  /** The COPY of the source's parent, or absent on the fork's root. */
  parentMessageId?: string;
  /** Recounted `0..n` over the copied messages, not inherited. */
  depth: number;
  /** The source's `metadata`, minus the streaming flag. */
  metadata: Record<string, unknown>;
}

/**
 * Flatten a fork prefix into the records a store is about to write.
 *
 * Shared rather than written twice because every rule in it is a rule a fork can
 * get subtly wrong in a way no type catches: an unremapped parent id silently
 * points the copy at the ORIGINAL chat's tree, an inherited `depth` breaks path
 * ordering the moment the fork is branched, and a carried `placeholder` flag
 * makes a finished message look like it is still streaming.
 *
 * `branchIndex` and `active` are not in the plan because they are constants —
 * `0` and `true` for every copy, since a fork is one straight line.
 */
export function planForkedMessages(
  prefix: readonly MessageRecord[],
  mintId: () => string,
): ForkedMessagePlan[] {
  const plans: ForkedMessagePlan[] = [];
  let previousId: string | undefined;
  for (const source of prefix) {
    const id = mintId();
    const metadata = { ...source.metadata };
    delete metadata[PLACEHOLDER_KEY];
    // The parent is the PREVIOUS COPY, not a remap of the source's own parent:
    // the prefix is a path, so they are the same message — except where a
    // placeholder was dropped out of the middle, and there re-linking across the
    // gap is exactly what flattening means. Remapping the source's parent would
    // instead leave that copy pointing at a message the fork does not contain.
    plans.push({
      id,
      source,
      ...(previousId === undefined ? {} : { parentMessageId: previousId }),
      depth: plans.length,
      metadata,
    });
    previousId = id;
  }
  return plans;
}

/**
 * The forked chat's title. A chat that never had one still does not get one —
 * `"Fork of "` alone reads as a bug, and a host that titles its chats lazily
 * would rather keep the empty title than inherit a placeholder.
 */
export function forkedChatTitle(title: string | undefined): string | undefined {
  return title === undefined ? undefined : `Fork of ${title}`;
}

/**
 * The text a search index sees for one message body: a string as itself, a
 * parts body as ALL of its text parts joined with a newline.
 *
 * ALL of them, and that word is the whole reason this is a named function
 * rather than an expression inlined into two adapters. The system this design
 * is copied from indexed only the FIRST text part of a multipart message, which
 * is invisible until the day someone searches for a phrase that happened to
 * land in paragraph two and concludes the feature is broken. Stating the
 * projection once — and having the sqlite adapter's SQL written against this
 * definition — is what keeps the two implementations from disagreeing about
 * what "the text of a message" means.
 *
 * Non-text parts contribute nothing: an image's base64 payload is not prose,
 * and indexing it would bloat the index with tokens no human will ever type.
 */
export function searchTextOf(content: AiMessageContent): string {
  if (typeof content === "string") return content;
  return content
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join(SEARCH_TEXT_SEPARATOR);
}

/**
 * Reject a `listMessages` call that named BOTH paging cursors.
 *
 * Shared rather than re-decided per adapter for the reason everything else here
 * is shared: the two cursors page in opposite directions, so "both" is a range
 * read with its own unanswered question — which end does `limit` count from? —
 * and two adapters each guessing an answer is two different pagers behind one
 * port. Refusing costs the caller one line and removes the ambiguity for good.
 */
export function assertListMessagesCursors(
  opts:
    | Pick<ListMessagesOptions, "afterOrderKey" | "beforeOrderKey">
    | undefined,
): void {
  if (opts?.afterOrderKey !== undefined && opts.beforeOrderKey !== undefined) {
    throw new AgentKitHostError(
      "invalid_cursor",
      "listMessages takes afterOrderKey or beforeOrderKey, not both.",
      {
        afterOrderKey: opts.afterOrderKey,
        beforeOrderKey: opts.beforeOrderKey,
      },
    );
  }
}

/**
 * One imported message, resolved into the record fields the store will write.
 *
 * `orderKey`, `depth` and `branchIndex` are here — and NOT in
 * {@link ImportMessageInput} — because they are derived, and the derivation is
 * the same one every append in this port already performs. An import that could
 * name them could write a chat no sequence of appends could have produced, and
 * the very next append onto that chat would have to reconcile the difference.
 */
export interface ImportedMessagePlan {
  input: ImportMessageInput;
  /** Creation order, `1..n` — exactly what a run of appends would have assigned. */
  orderKey: number;
  depth: number;
  branchIndex: number;
  /** The resolved link; absent on a root (`undefined` and `null` both mean root). */
  parentMessageId?: string;
  /** The caller's bag, plus `internal: true` when the shorthand asked for it. */
  metadata: Record<string, unknown>;
  createdAt: string;
}

/** Every {@link InvalidImportError} this module raises, as `details.reason`. */
export type ImportRejectionReason =
  | "duplicate_message_id"
  | "unknown_parent"
  | "forward_parent"
  | "no_active_path"
  | "broken_active_chain"
  | "active_leaf_has_child";

function rejectImport(
  reason: ImportRejectionReason,
  message: string,
  details: Record<string, unknown>,
): never {
  throw new InvalidImportError(message, { reason, ...details });
}

/** Roots share one bucket in the sibling counter; no id can collide with it. */
const ROOT_PARENT_KEY = " root";

/**
 * Validate an import payload IN FULL and resolve every derived field, or throw.
 *
 * Pure and synchronous, like everything else in this module, so an adapter can
 * call it BEFORE it opens a transaction and know that the write phase which
 * follows cannot fail on the data. That ordering is what makes "all-or-nothing"
 * true even in the Map-backed adapter, which has no rollback to fall back on.
 *
 * The duplicate CHAT id is not checked here: only the store knows what it
 * already holds, so it raises that one itself (`reason: "duplicate_chat"`).
 */
export function planImportedMessages(
  messages: readonly ImportMessageInput[],
  chatId: string,
  defaultCreatedAt: string,
): ImportedMessagePlan[] {
  const declaredIds = new Set(messages.map((message) => message.id));
  const plans: ImportedMessagePlan[] = [];
  const planById = new Map<string, ImportedMessagePlan>();
  // Max-plus-one, specialized: an import builds each parent's children densely
  // from 0 in creation order and never deletes, so the count of siblings
  // already placed IS what `nextBranchIndex` would return over them.
  const siblingCounts = new Map<string, number>();
  /** Children per parent — what proves the active chain ends at a LEAF. */
  const childCounts = new Map<string, number>();

  for (const input of messages) {
    if (planById.has(input.id)) {
      rejectImport(
        "duplicate_message_id",
        `Import for chat ${chatId} names message ${input.id} twice.`,
        { chatId, messageId: input.id },
      );
    }
    const parentId = input.parentMessageId ?? undefined;
    let parent: ImportedMessagePlan | undefined;
    if (parentId !== undefined) {
      parent = planById.get(parentId);
      if (parent === undefined) {
        // Two honest reasons, told apart by whether the id is in the payload at
        // all: a typo and a payload sorted the wrong way are different bugs,
        // and an importer fixing the second does not want to go hunting for the
        // first.
        rejectImport(
          declaredIds.has(parentId) ? "forward_parent" : "unknown_parent",
          declaredIds.has(parentId)
            ? `Import for chat ${chatId}: message ${input.id} names parent ${parentId}, which appears later in the list. Messages must be in creation order.`
            : `Import for chat ${chatId}: message ${input.id} names parent ${parentId}, which the payload does not contain.`,
          { chatId, messageId: input.id, parentMessageId: parentId },
        );
      }
      childCounts.set(parentId, (childCounts.get(parentId) ?? 0) + 1);
    }
    const siblingKey = parentId ?? ROOT_PARENT_KEY;
    const branchIndex = siblingCounts.get(siblingKey) ?? 0;
    siblingCounts.set(siblingKey, branchIndex + 1);
    const plan: ImportedMessagePlan = {
      input,
      orderKey: plans.length + 1,
      depth: parent === undefined ? 0 : parent.depth + 1,
      branchIndex,
      ...(parentId === undefined ? {} : { parentMessageId: parentId }),
      metadata: {
        ...(input.metadata ?? {}),
        ...(input.internal === true ? { [INTERNAL_KEY]: true } : {}),
      },
      createdAt: input.createdAt ?? defaultCreatedAt,
    };
    plans.push(plan);
    planById.set(plan.input.id, plan);
  }

  assertOneActiveChain(plans, childCounts, chatId);
  return plans;
}

/**
 * The invariant the rest of this port maintains by construction, checked once
 * over a payload that arrived all at once: the active messages form EXACTLY one
 * chain from a root to a childless leaf.
 *
 * Every way to break it collapses to a link that does not hold when the active
 * set is read in path order — two active children of one message put a sibling
 * where a child belongs, an active message under an inactive parent puts a
 * stranger at the head of the chain, a second active root does the same at
 * depth 0. So one walk decides all of them, and the leaf check is the one extra
 * fact a walk of the active set alone cannot see.
 */
function assertOneActiveChain(
  plans: readonly ImportedMessagePlan[],
  childCounts: ReadonlyMap<string, number>,
  chatId: string,
): void {
  if (plans.length === 0) return;
  const path = plans
    .filter((plan) => plan.input.active)
    .sort((a, b) => a.depth - b.depth || a.orderKey - b.orderKey);
  if (path.length === 0) {
    rejectImport(
      "no_active_path",
      `Import for chat ${chatId} has ${plans.length} messages and none of them active; a conversation with no active path reads as empty.`,
      { chatId, messageCount: plans.length },
    );
  }
  for (const [index, plan] of path.entries()) {
    const expectedParent = index === 0 ? undefined : path[index - 1]?.input.id;
    if (plan.parentMessageId !== expectedParent) {
      rejectImport(
        "broken_active_chain",
        `Import for chat ${chatId}: active message ${plan.input.id} follows ${expectedParent ?? "the start of the path"} but names parent ${plan.parentMessageId ?? "none"}. The active messages must be one root-to-leaf chain.`,
        {
          chatId,
          messageId: plan.input.id,
          parentMessageId: plan.parentMessageId ?? null,
          expectedParentMessageId: expectedParent ?? null,
        },
      );
    }
  }
  const leaf = path[path.length - 1];
  if (leaf !== undefined && (childCounts.get(leaf.input.id) ?? 0) > 0) {
    rejectImport(
      "active_leaf_has_child",
      `Import for chat ${chatId}: the active path ends at ${leaf.input.id}, which has children. The active chain must run to a childless leaf.`,
      { chatId, messageId: leaf.input.id },
    );
  }
}

/**
 * Whether a message is excluded from search hits: replay bookkeeping the user
 * never saw, or an answer that has not been written yet.
 *
 * Read at QUERY time rather than at index time, in both adapters, because both
 * flags are `metadata` — and `metadata` is rewritten after the fact. A
 * placeholder becomes a real answer when its run finishes, and an index that
 * had decided at insert time would keep the finished answer permanently
 * unfindable.
 */
export function isSearchableMetadata(
  metadata: Record<string, unknown>,
): boolean {
  return metadata[INTERNAL_KEY] !== true && metadata[PLACEHOLDER_KEY] !== true;
}
