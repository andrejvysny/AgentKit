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
import { AgentKitHostError, InvalidForkPointError } from "../errors.js";
import type {
  AppendMessageInput,
  MessageRecord,
} from "../ports/conversation-store.js";
import { orderMessagesForProvider } from "../turn/message-order.js";

/** `metadata` key marking an answer still streaming; never copied by a fork. */
const PLACEHOLDER_KEY = "placeholder";

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
