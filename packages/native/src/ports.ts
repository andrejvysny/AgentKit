/**
 * Store port types `NativeAssistantStore` implements, owned by this package.
 *
 * Types only: the TypeScript host these used to come from (`@agentkit/host`)
 * is gone, so the port shapes the binding maps onto live here. The
 * engine (Rust) owns every runtime rule; nothing in this file executes.
 */
import type {
  AiContextSizePreference,
  AiMessageContent,
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderContinuation,
  AiProviderModel,
  AiToolCall,
  AiToolResult,
  TaskEventEnvelope,
} from "@agentkit/contracts";

// ---- system ----
/** Wall-clock access. The only source of "now" the host layer is allowed. */
export interface Clock {
  now(): Date;
  /** ISO-8601 rendering of {@link now}; what every record persists. */
  nowIso(): string;
}

// ---- write-policy ----

/** Trusted invocation identity; actor grants never inherit chat-wide consent. */
export interface WriteInvocationScope {
  actorId?: string;
  /** Hash of validated tool arguments, excluding the idempotency key. */
  payloadFingerprint?: string;
  /** Exact revision consent covered; null means the host has no revision. */
  revision?: string | null;
}

/**
 * How much a chat may write without asking.
 *
 * - `auto_readonly_confirm_writes` — the default: reads run freely, writes stage
 *   a proposal and wait, unless the user granted a standing allowance for that
 *   tool + kind.
 * - `confirm_all_writes` — no allowance is ever honoured; every write waits.
 * - `auto_all` — everything auto-applies. For trusted, fully-undoable hosts.
 */
export type WritePolicyMode =
  | "auto_readonly_confirm_writes"
  | "confirm_all_writes"
  | "auto_all";

/**
 * A standing "yes" for one `(chat, tool, proposal kind[, scope])` combination,
 * up to a risk ceiling.
 *
 * The ceiling is what keeps the grant honest: a user who approved low-risk edits
 * for this tool has not thereby approved a destructive one, and a model cannot
 * escalate by re-labelling its own proposal.
 */
export interface WriteAllowance extends WriteInvocationScope {
  key: string;
  chatId: string;
  toolName: string;
  proposalKind: string;
  /**
   * What this grant is scoped to — the proposal's `scopeKey`, i.e. the thing
   * being written. See {@link AutoApplyQuery.scopeKey}.
   */
  scopeKey?: string;
  /** Highest risk covered; rank N covers everything ≤ N. */
  maxRisk: RiskLevel;
  createdAt: string;
}

export interface WriteAllowanceInput extends WriteInvocationScope {
  chatId: string;
  toolName: string;
  proposalKind: string;
  /**
   * Confine the grant to ONE scope. Absent, it covers every scope the tool can
   * reach from this chat — which is what a grant meant before this field
   * existed, and remains the meaning of one recorded without it.
   */
  scopeKey?: string;
  maxRisk: RiskLevel;
}

export interface AutoApplyQuery extends WriteInvocationScope {
  chatId: string;
  toolName: string;
  proposalKind: string;
  /**
   * The scope the staged proposal actually writes to.
   *
   * It matters because the scope comes from MODEL-SUPPLIED input
   * (`ProposalBuilderToolOptions.scopeKeyOf` derives it from the tool call), so
   * without it a "yes, edit this document" answered about document A is a
   * standing yes for the same tool writing document B. An allowance recorded
   * WITHOUT a `scopeKey` still matches any scope — that is what every grant
   * given before this field existed meant, and silently narrowing them would
   * turn working auto-apply into a wall of confirmations.
   */
  scopeKey?: string;
  risk: RiskLevel;
}

/**
 * Decides whether a staged write applies immediately or waits for a human.
 *
 * Synchronous by design: it is consulted inside a write tool's execution, on the
 * hot path of a model turn, and an answer that could block on IO would be an
 * answer that can time out — turning "needs confirmation" into "tool failed".
 */
export interface WritePolicy {
  mode(): WritePolicyMode;
  isAutoApplyAllowed(query: AutoApplyQuery): boolean;
  allow(input: WriteAllowanceInput): WriteAllowance;
  revoke(chatId: string, key: string): void;
  list(chatId: string): WriteAllowance[];
  /** Revoke ephemeral grants when a trusted invocation actor disconnects. */
  revokeActor?(actorId: string): void;
}

// ---- proposal-store ----
/**
 * Lifecycle of a staged write.
 *
 * `pending → approved → applying → applied|failed` is the whole happy path;
 * `pending` also exits to `rejected` (a human said no) and `invalidated` (the
 * world moved on, so what was reviewed is no longer what would be written).
 *
 * `applying` is a persisted state, not a variable in a function: a process that
 * dies mid-apply leaves the record there, and recovery can then ask the applier
 * whether the side effects landed instead of guessing.
 */
export type ProposalStatus =
  | "pending"
  | "approved"
  | "applying"
  | "applied"
  | "failed"
  | "rejected"
  | "invalidated";

/**
 * How bad it would be if this write were wrong. Ordered low → destructive; the
 * write policy grants allowances by rank, and rank N covers everything ≤ N.
 */
export type RiskLevel = "low" | "medium" | "high" | "destructive";

/** Who decided, and on what authority. Persisted so an audit can tell them apart. */
export interface ProposalDecision {
  /**
   * `"policy"` decisions are machine approvals under a standing allowance and
   * MUST carry `policyId`. Recording one as a user decision would let an
   * auto-applied write masquerade as human-reviewed.
   */
  actor: "user" | "policy";
  decidedBy?: string;
  policyId?: string;
  reason?: string;
  decidedAt: string;
}

export interface ProposalRecord {
  id: string;
  chatId: string;
  runId?: string;
  /** What this write touches; the serialization + idempotency namespace. */
  scopeKey: string;
  /** Model-supplied idempotency key. Unique per scope when present. */
  actionId?: string;
  toolName: string;
  /** Host-defined proposal kind (the write's shape), e.g. "document.edits". */
  kind: string;
  risk: RiskLevel;
  status: ProposalStatus;
  /** The full staged payload, as the tool built it. */
  envelope: Record<string, unknown>;
  operations: unknown[];
  warnings: string[];
  /** The builder dropped operations to fit a cap; never auto-apply a truncated write. */
  truncated: boolean;
  /** Scope revision the proposal was built against; the staleness check. */
  revisionAtCreate?: string;
  /**
   * The apply attempt that claimed this proposal (written at the claim, before
   * any side effect). Crash recovery reads it to ask the applier what happened;
   * without it, an interrupted `applying` record is unresolvable.
   */
  operationId?: string;
  decision?: ProposalDecision;
  /**
   * Why this proposal ended where it did (`revision_conflict`, `interrupted`, an
   * applier's error). Written by the transition that terminates it, and read
   * back by the UI and by the model correcting its own work — "failed" without a
   * reason is not actionable by either.
   */
  reason?: string;
  createdAt: string;
  decidedAt?: string;
  /**
   * When the apply claim (`approved → applying`) was taken; the reconcile
   * window keys on it.
   *
   * The other stamps cannot stand in for it: `decidedAt` is when a human said
   * yes, which for a write applied much later is far older than the claim, so a
   * live apply measured by it looks stuck. Absent on records written before the
   * field existed and on anything that never reached `applying`.
   */
  claimedAt?: string;
  appliedAt?: string;
}

/**
 * What one apply attempt did, keyed by operation id and persisted verbatim.
 *
 * `partial` is a real outcome, not a rounding of success or failure: some
 * operations landed and some did not, and both halves matter to the model that
 * has to correct the rest. The state machine still ends at `applied` (the write
 * happened); the partiality lives here.
 */
export interface ApplyOutcome {
  status: "applied" | "partial" | "failed";
  appliedOps: number;
  failedOps: { opIndex: number; error: string }[];
  /** Host payload describing the result (ids created, etc.). */
  resultJson?: string;
  /** Scope revision AFTER the apply. */
  revision?: string;
}

export interface CreateProposalInput {
  id: string;
  chatId: string;
  runId?: string;
  scopeKey: string;
  actionId?: string;
  toolName: string;
  kind: string;
  risk: RiskLevel;
  envelope: Record<string, unknown>;
  operations: unknown[];
  warnings: string[];
  truncated: boolean;
  revisionAtCreate?: string;
  createdAt: string;
}

/** Fields a transition may write alongside the status change, atomically. */
export interface ProposalPatch {
  decision?: ProposalDecision;
  decidedAt?: string;
  /**
   * When the apply claim (`approved → applying`) was taken; the reconcile
   * window keys on it. Written by the transition that takes the claim — see
   * {@link ProposalRecord.claimedAt}.
   */
  claimedAt?: string;
  appliedAt?: string;
  operationId?: string;
  /** Why it failed / was invalidated; surfaced to the model and the UI. */
  reason?: string;
}

export interface ListProposalsOptions {
  status?: ProposalStatus;
  limit?: number;
}

export interface ProposalStore {
  /**
   * MUST enforce UNIQUE `(scopeKey, actionId)` when `actionId` is set and throw
   * {@link DuplicateActionIdError} on collision — that pair IS the idempotency
   * guarantee a write tool advertises to the model.
   *
   * The constraint covers every status except
   * {@link ACTION_ID_RELEASING_STATUSES} (a SQL adapter expresses this as a
   * partial unique index).
   */
  create(input: CreateProposalInput): Promise<ProposalRecord>;
  get(proposalId: string): Promise<ProposalRecord | null>;
  /**
   * The MOST RECENT proposal carrying this key in this scope. Recency matters
   * because a released key can be re-used (see
   * {@link ACTION_ID_RELEASING_STATUSES}): the dedup decision must reflect the
   * latest attempt, not the rejected one it superseded.
   */
  getByActionId(
    scopeKey: string,
    actionId: string,
  ): Promise<ProposalRecord | null>;
  listByChat(
    chatId: string,
    opts?: ListProposalsOptions,
  ): Promise<ProposalRecord[]>;
  /**
   * Cross-chat status query. Crash recovery needs "every proposal stuck in
   * `applying`", which no per-chat listing can answer.
   */
  listByStatus(
    status: ProposalStatus,
    opts?: { limit?: number },
  ): Promise<ProposalRecord[]>;

  /**
   * Compare-and-set the proposal's status. MUST reject with
   * {@link InvalidProposalTransitionError} when the current status is not in
   * `from`, or when the change is not in `PROPOSAL_TRANSITIONS`.
   */
  transition(
    proposalId: string,
    from: ProposalStatus[],
    to: ProposalStatus,
    patch?: ProposalPatch,
  ): Promise<ProposalRecord>;

  /**
   * Record what an apply attempt did. Idempotent on `operationId`: a second call
   * with the same id MUST keep the first outcome and return it, because the id
   * identifies work that happened once and a "corrected" outcome would erase the
   * evidence a replay depends on.
   */
  recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome>;
  getOutcome(operationId: string): Promise<ApplyOutcome | null>;

  /**
   * Invalidate every still-`pending` proposal in a scope because the scope moved
   * to `newRevision`. Returns how many were invalidated.
   */
  invalidatePendingForRevision(
    scopeKey: string,
    newRevision: string,
  ): Promise<number>;

  /**
   * Delete every proposal a chat staged, along with the apply outcomes those
   * proposals claimed, and return how many PROPOSALS were removed.
   *
   * BY CHAT, NOT BY SCOPE, and that is the substantive choice here. A proposal
   * carries both: `chatId` is the conversation it was staged from, `scopeKey`
   * is the thing it writes to — and those are not the same set. Two chats
   * routinely propose writes to one shared document, so deleting "the scope" of
   * a chat being removed would take another conversation's staged writes with
   * it, silently. `chatId` is what identifies A CHAT's proposals, so `chatId` is
   * what this takes.
   *
   * Outcomes go with their proposals because an `ApplyOutcome` is keyed by an
   * `operationId` that only ever appears on the proposal that claimed it: left
   * behind, it is a row describing work nothing can name — unreadable by
   * `getOutcome` (which is asked for an id nobody holds any more) and
   * indistinguishable from a leak.
   *
   * Unconditional, like `TaskStore.deleteByScope`: a `pending` proposal is
   * deleted as readily as an applied one. Whether live work may be discarded is
   * the caller's decision — `ConversationService.deleteChat` makes it — and a
   * store that second-guessed it would leave a chat that cannot be deleted
   * because a model once staged a write nobody ever answered.
   */
  deleteByChat(chatId: string): Promise<number>;
}

// ---- conversation-store ----

/** A conversation thread. Chats own messages; runs are attached to a chat. */
export interface ChatRecord {
  id: string;
  title?: string;
  createdAt: string;
  updatedAt: string;
  metadata: Record<string, unknown>;
  /**
   * Hidden from the default listing, but otherwise a perfectly ordinary chat:
   * it still answers {@link ConversationStore.getChat}, still accepts appends,
   * still comes back from an explicit `ids` fetch.
   *
   * A first-class column rather than a `metadata` flag because
   * {@link ListChatsOptions.includeArchived} filters on it, and a store cannot
   * index — or a caller reason about — a key buried in an opaque JSON bag the
   * host is otherwise free to put anything in.
   *
   * `false` on every chat this store creates, and on every fork: archiving is a
   * statement about one conversation, and a copy of an archived chat is a new
   * conversation somebody just made on purpose.
   */
  archived: boolean;
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
 *   `afterOrderKey` cursor pages forward correctly WITHIN one path. It is a
 *   cursor into a path, not into a chat: see
 *   {@link ListMessagesOptions.afterOrderKey} for what a branch switch does to
 *   one taken before it.
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
  /**
   * The message body: a plain string, or the ordered content parts of a
   * multimodal message (`AiMessageContent` — see
   * `packages/contracts/src/content.ts`).
   *
   * A store round-trips parts LOSSLESSLY and inspects nothing inside them. In
   * particular an image part may name a host attachment
   * (`source: { kind: "ref", ref }`) rather than carry its bytes: the ref is
   * what is persisted, and `TurnRunner` resolves it to inline data per provider
   * pass without ever rewriting this record (see
   * {@link AttachmentResolver}). A store that "helpfully" inlined the bytes
   * would make every fork and every page of the conversation carry them.
   *
   * `role: "tool"` and `role: "system"` records are strings by construction — a
   * tool result is a serialized envelope, a system record is a prompt or a UI
   * banner — and nothing in the host writes parts on either.
   */
  content: AiMessageContent;
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
  /** Archived messages remain available by id for audit, never for replay. */
  archived?: boolean;
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
  content: MessageRecord["content"];
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
   *
   * - **Null** — create another root and activate its path. Explicit root edits
   *   must not append to whichever leaf happens to be active.
   *
   * See {@link AppendMessageInput.activate} for the third case: naming a parent
   * WITHOUT switching to it.
   */
  parentMessageId?: string | null;
  /**
   * Whether this append also makes the new message's path the active one.
   * Default `true` — every case above.
   *
   * `false` is a CHAIN APPEND, and it exists for exactly one caller: a run
   * writing the records of the turn it is already executing. `branchIndex` is
   * assigned as usual, but `active` is INHERITED from the named parent and no
   * path switch happens — the record lands where the run's own work is, not
   * wherever the conversation happens to be pointing at the instant the write
   * lands. Inherited precisely: `active` is true when the parent is active AND
   * still the end of the live chain, and false otherwise (see THE INVARIANT
   * below).
   *
   * WHY. A turn writes its internal assistant record, then each tool result,
   * over seconds or minutes, and a user may switch branches in the middle of
   * that. An append that takes "the active leaf" as its parent would hang the
   * second half of one run's records off the branch the user just moved to:
   * foreign tool results appear in a conversation that never ran them, and the
   * run's own branch is left with an assistant turn whose `tool_calls` were
   * never answered — which the next replay of that branch has to reconcile as a
   * synthetic failure for a tool that actually succeeded. Chaining off the id
   * the run last wrote removes the race instead of narrowing it.
   *
   * THE INVARIANT — one root-to-CHILDLESS-leaf active chain — holds in every
   * case, which is why the inheritance is stated as a rule rather than as a
   * copy of one flag:
   *
   * - Parent is the current active leaf: `active: true`, and the new record is
   *   itself childless. The same chain, one message longer — byte-identically
   *   what the unparented append it replaces would have produced. This is the
   *   ordinary case, the one a run takes when nobody touches the chat.
   * - Parent is inactive: `active: false`, and no flag anywhere changes. The
   *   abandoned branch grows and the live path does not notice. This is the
   *   case a mid-run branch switch produces.
   * - Parent is active but ALREADY HAS an active child — someone appended to
   *   the conversation between two of this run's writes: `active: false`. The
   *   chain this record belongs to was continued by somebody else, so the
   *   record is superseded rather than live, and inheriting `true` here would
   *   give one message two active children, which is not a path. Same outcome
   *   as a branch switch, reached a different way.
   *
   * `activate: false` WITHOUT `parentMessageId` is rejected. There is nothing
   * to chain off: the only parent an unparented append could take is the active
   * leaf, which is the race this flag exists to avoid, so a caller that means
   * "continue the conversation" must say `activate: true` (or nothing) and one
   * that means "continue MY chain" must name the link.
   */
  activate?: boolean;
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
  content?: MessageRecord["content"];
  metadata?: Record<string, unknown>;
  toolCalls?: AiToolCall[];
}

/**
 * Fields a chat may change after it was created.
 *
 * `metadata` REPLACES the stored bag rather than merging into it — the same
 * rule as {@link UpdateMessagePatch.metadata}, for the same reason: a merge
 * makes "unset this flag" unexpressible, and a caller that wanted a merge
 * already has the record it read to build one from.
 *
 * Every write here bumps `updatedAt`, which is `listChats`' sort key: a renamed
 * or archived chat moves to the front of the listing, exactly as one that just
 * received a message does. There is one `updatedAt`, and a second "touched but
 * do not re-sort" notion of modification would be a field this record does not
 * have.
 */
export interface UpdateChatPatch {
  title?: string;
  metadata?: Record<string, unknown>;
  archived?: boolean;
}

export interface ListMessagesOptions {
  /**
   * How many messages to return.
   *
   * With no cursor, or with {@link ListMessagesOptions.afterOrderKey}: the most
   * recent N (still in ascending path order). With
   * {@link ListMessagesOptions.beforeOrderKey}: the N immediately before that
   * key — the page above the one already on screen.
   */
  limit?: number;
  /**
   * Only messages with `orderKey` greater than this.
   *
   * Correct WITHIN a path, and only there. `orderKey` rises with depth along
   * any one chain, so paging forward through the path a cursor was taken on
   * behaves exactly as it did before a chat could branch.
   *
   * ACROSS a switch it does not, and cannot: activating an older branch makes
   * the live path one whose keys are all BELOW a cursor taken on the branch
   * just left, so the next page reads empty and looks like "no new messages"
   * rather than "a different conversation". A client that can switch branches
   * — its own switch, or another client's — must notice the path changed and
   * re-list from the start instead of continuing the cursor; comparing the
   * active leaf's id between reads is the cheap way to see it.
   */
  afterOrderKey?: number;
  /**
   * Only messages with `orderKey` STRICTLY LESS than this — the `limit` of them
   * nearest the key, still returned in ascending path order.
   *
   * This is the scroll-back cursor, and it is a separate option rather than a
   * sign convention on `afterOrderKey` because the two page in opposite
   * directions and a client that confused them would silently read the wrong
   * end of the conversation. The pattern it serves: open a chat by listing the
   * last page (`limit` alone), then walk upwards by passing the first returned
   * message's `orderKey` here, again and again, until a page comes back short.
   *
   * Same path-scoped caveat as {@link ListMessagesOptions.afterOrderKey}: it is
   * a cursor into a path, not into a chat, and a branch switch invalidates it.
   *
   * Setting BOTH cursors is rejected (`invalid_cursor`) rather than
   * intersected. A range read is a third operation with its own paging
   * semantics — which end does `limit` count from? — and guessing one would
   * make the answer depend on which adapter is underneath.
   */
  beforeOrderKey?: number;
}

export interface ListChatsOptions {
  limit?: number;
  /** Chats updated before this ISO timestamp (keyset pagination). */
  before?: string;
  /**
   * Fetch exactly these chats by id, in the same order and paging as the
   * default listing (`updatedAt` descending, `before`/`limit` still applied).
   *
   * A batch `getChat`, not a filter over a browse: an id a caller already holds
   * came from somewhere — a task payload, a link, a client-side cache — and
   * naming it is a claim that this chat specifically is wanted. So an id
   * resolves whatever the chat's state is, {@link ListChatsOptions.includeArchived}
   * included; archiving hides a chat from browsing, not from anyone who can
   * already name it.
   *
   * An empty array returns nothing, which is the only reading that composes:
   * "the chats among these zero ids" is empty, and treating it as "no filter"
   * would turn a caller's emptied selection into the whole listing.
   */
  ids?: string[];
  /**
   * Whether archived chats appear. Default `false` — they do not.
   *
   * The default is the exclusion because that is what archiving is FOR: a chat
   * list that still showed everything after an archive would make the feature
   * do nothing a client could see. `ids` overrides it (see above).
   */
  includeArchived?: boolean;
}

/** One message matching a {@link ConversationStore.searchMessages} query. */
export interface MessageSearchHit {
  chatId: string;
  messageId: string;
  /**
   * A window of the message's text around the match, with the matched terms
   * wrapped in {@link SEARCH_MATCH_START}/{@link SEARCH_MATCH_END} and elided
   * text standing in as {@link SEARCH_SNIPPET_ELLIPSIS}.
   *
   * A snippet rather than the message: a search result list renders the
   * evidence, and returning whole bodies would make one query carry every
   * attachment-bearing message it happened to match.
   */
  snippet: string;
}

export interface SearchMessagesOptions {
  /** Restrict to one chat. Absent searches every chat in the store. */
  chatId?: string;
  /** Default {@link DEFAULT_SEARCH_LIMIT}. */
  limit?: number;
}

/** The chat half of an {@link ImportConversationInput}. */
export interface ImportChatInput {
  /** Preserved verbatim; an id this store already has is `invalid_import`. */
  id: string;
  title?: string;
  metadata?: Record<string, unknown>;
  archived?: boolean;
  /** Defaults to now. Also becomes the chat's `updatedAt` — see the method doc. */
  createdAt?: string;
}

/**
 * One message of an import, in CREATION ORDER.
 *
 * The caller supplies identity, the tree links, the `active` flags and the tool
 * linkage; the STORE assigns `orderKey`, `depth` and `branchIndex`. That split is the whole
 * design: those three are derived facts with rules
 * (`packages/host/src/conversation/message-tree.ts`) that every append in this
 * port already obeys, and letting an import name them would let it write a chat
 * no append could have produced — which the very next append would then have to
 * reconcile.
 */
export interface ImportMessageInput {
  id: string;
  role: MessageRecord["role"];
  content: AiMessageContent;
  /**
   * Tool linkage, carried through the import VERBATIM — a store writes these
   * three exactly as given and derives nothing from them.
   *
   * Optional, and absent on the overwhelming majority of an import's messages;
   * present on the two kinds that are useless without it. A `role: "tool"`
   * record answers a specific call (`toolCallId`), and the internal assistant
   * turn that asked for it declares the ids it is waiting on (`toolCalls`).
   * That pairing is the ONLY thing `orderMessagesForProvider` has to work
   * with — it groups an assistant turn with its own results by matching ids,
   * not by kind — so an import that dropped it would migrate a conversation
   * whose every replay hands the provider a tool result with no preceding
   * `tool_calls`, which providers reject outright. Unlike `orderKey`/`depth`/
   * `branchIndex`, none of the three is derivable from the payload's shape:
   * they are facts about what the model said, and only the caller has them.
   *
   * `modelResultJson` is the slim model-facing envelope a replay sends in place
   * of the full tool payload; see {@link MessageRecord.modelResultJson}.
   */
  toolCallId?: string;
  toolCalls?: AiToolCall[];
  modelResultJson?: string;
  /** Absent or `null` makes a root. Must name a message EARLIER in the list. */
  parentMessageId?: string | null;
  /** Whether this message is on the imported chat's active path. */
  active: boolean;
  /**
   * Shorthand for `metadata.internal = true` — a replay-only record. Merged
   * into {@link ImportMessageInput.metadata} rather than replacing it, because
   * an importer that has both is expressing two different things.
   */
  internal?: boolean;
  /** Archived ancestors archive their descendants; active archived imports fail. */
  archived?: boolean;
  metadata?: Record<string, unknown>;
  /** Defaults to the chat's `createdAt`. */
  createdAt?: string;
}

export interface ImportConversationInput {
  chat: ImportChatInput;
  /** In creation order: a parent MUST appear before any child that names it. */
  messages: ImportMessageInput[];
}

/** What {@link ConversationStore.forkChat} wrote: the new chat and its copy. */
export interface ForkChatResult {
  chat: ChatRecord;
  /** The copied messages, root first — the fork's whole active path. */
  messages: MessageRecord[];
}

/** Checked inside the same operation as each run-owned message mutation. */
export interface RunWriteFence {
  taskId: string;
  leaseToken: string;
}

export interface ConversationStore {
  createChat(input: CreateChatInput): Promise<ChatRecord>;
  getChat(chatId: string): Promise<ChatRecord | null>;
  listChats(opts?: ListChatsOptions): Promise<ChatRecord[]>;
  /**
   * Change a chat's title, metadata bag, or archived flag, and answer with the
   * updated record. Unknown chat → `RecordNotFoundError`.
   *
   * `metadata` REPLACES; `updatedAt` moves. See {@link UpdateChatPatch}.
   */
  updateChat(chatId: string, patch: UpdateChatPatch): Promise<ChatRecord>;
  /**
   * Delete the chat and EVERY message in it, transactionally — including
   * messages on branches the conversation is not currently showing, which are
   * as much a part of this chat as the live path is.
   *
   * Unknown chat → `RecordNotFoundError`, rather than a silent success. A
   * delete that cannot say whether it deleted anything is a delete a caller
   * cannot build a confirmation dialog on top of.
   *
   * SCOPED TO THE CONVERSATION, and deliberately no further: the tasks that ran
   * in this chat and the proposals staged from it live in other stores, and a
   * conversation store that reached into them would be making a policy decision
   * (what a delete means) that belongs one layer up. `ConversationService.deleteChat`
   * is the operation that composes all three — and the one that refuses while a
   * run is still live.
   *
   * A store maintaining a search index MUST leave no residue behind: a hit
   * pointing at a deleted chat is worse than a missing hit, because a client
   * cannot tell it from a permissions bug.
   */
  deleteChat(chatId: string): Promise<void>;
  /**
   * Append a message, assigning the next `orderKey` for its chat and placing it
   * in the chat's tree (see {@link AppendMessageInput.parentMessageId}).
   * Assigning the key in the store (not the caller) is what makes concurrent
   * appends from a run and from a user land in a defined order.
   *
   * A branching append — one that names a parent — is ATOMIC with the path
   * switch it causes: either the branch exists and is active, or neither.
   *
   * `activate: false` suppresses that switch entirely; see
   * {@link AppendMessageInput.activate}.
   */
  appendMessage(
    input: AppendMessageInput,
    fence?: RunWriteFence,
  ): Promise<MessageRecord>;
  updateMessage(
    messageId: string,
    patch: UpdateMessagePatch,
    fence?: RunWriteFence,
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
   * `limit` still means "the most recent N", `afterOrderKey` still pages
   * forward, and `beforeOrderKey` pages BACKWARDS from a key the caller already
   * holds; all three apply to the active path, and all three remain well-defined
   * because `orderKey` increases with depth along any path (see
   * {@link MessageRecord}). Passing both cursors is `invalid_cursor`.
   */
  listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]>;
  /** Fetch one record across branches; projection replay uses its stable identity. */
  getMessage(messageId: string): Promise<MessageRecord | null>;
  /** Full creation-ordered tree, without a hidden page limit. Audit may include archives. */
  listTree?(
    chatId: string,
    options?: { includeArchived?: boolean },
  ): Promise<MessageRecord[]>;
  /** Archive an inactive subtree atomically, retaining its records and branch indices. */
  archiveBranch?(messageId: string): Promise<{ archivedCount: number }>;
  /**
   * The DEEPEST message a given run wrote in a chat, or `null` when the run has
   * written nothing there.
   *
   * "Deepest" is `(depth DESC, orderKey DESC)` — the end of the chain this run
   * built, whether or not that chain is still the active path. It is NOT scoped
   * to the active path on purpose: a run whose branch was abandoned mid-turn
   * must keep writing onto its own branch, and a lookup that only saw the live
   * path would hand it a link into somebody else's conversation.
   *
   * THE CALLER IS A SECOND ATTEMPT. `TurnRunner` seeds
   * `RunProjectionState.lastMessageId` from this on every attempt, so attempt 2
   * of a crashed turn CONTINUES attempt 1's chain instead of starting a second
   * one off the placeholder. That distinction is not cosmetic: by the time
   * attempt 2 runs, the placeholder already HAS an active child (attempt 1's
   * internal assistant record), and a chain append under a parent that already
   * has an active child lands `active: false` — so attempt 2's tool calls, its
   * tool results and everything after them would be written off the path that
   * every later turn replays. See {@link AppendMessageInput.activate}.
   *
   * On attempt 1 it answers the placeholder itself (the submit writes it with
   * `runId` set), which is exactly the seed the projector uses anyway — so the
   * ordinary turn is unchanged. A host executor that never wrote a record
   * carrying `runId` gets `null` and the same fallback.
   */
  lastMessageOfRun(
    chatId: string,
    runId: string,
  ): Promise<MessageRecord | null>;
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
   * Make a message's path the chat's active one, atomically, and answer with
   * that path — exactly what {@link ConversationStore.listMessages} would
   * return next, root first.
   *
   * Returning it is not a convenience: the switch already computes the set it
   * is about to write, so handing it back costs nothing, while a caller that
   * re-read the chat afterwards would be reading OUTSIDE the transaction — and
   * could report a path a concurrent append had already moved on from.
   *
   * What becomes active: the message, every ancestor of it, and then a descent
   * to a leaf — at each step preferring the child that is ALREADY active, and
   * falling back to the lowest `branchIndex` when none is. Everything else in
   * the chat becomes inactive.
   *
   * The preference only has something to prefer when the named message is
   * ALREADY on the current active path — re-activating an ancestor of where the
   * conversation already is walks back down the same branch. Switching AWAY
   * from a branch clears every flag in the abandoned subtree in the same
   * transaction, so switching back later does not resume where it left off:
   * there is nothing marked active left to prefer, and the descent falls back
   * to the lowest `branchIndex`. The flag is the path, not a per-node bookmark.
   *
   * Atomic, and that is the whole point of it being a port method rather than a
   * loop over `updateMessage`: a half-applied switch is a chat with two active
   * leaves, or none, and `listMessages` cannot report either as a conversation.
   */
  activatePath(messageId: string): Promise<MessageRecord[]>;
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
   *
   * And the copy is written in PROVIDER order, not in the source's write order.
   * A source chat creates its visible answer first, as an empty placeholder at
   * submit time, so that record carries a LOWER `orderKey` than the internal
   * assistant turn and the tool results that produced it; `runId` is what tells
   * a replay how to put the three back in the order a provider accepts, and a
   * fork drops it. So the repair happens once, here, and `depth`/`orderKey`/the
   * parent chain are all assigned on the repaired sequence — otherwise the fork
   * would replay a tool result before its own tool call, permanently, with
   * nothing left to fix it with.
   */
  forkChat(chatId: string, fromMessageId: string): Promise<ForkChatResult>;
  /**
   * Write a whole conversation — the chat and its messages — in ONE
   * transaction, PRESERVING the ids the caller supplies.
   *
   * This is the history-migration primitive, and id preservation is the entire
   * point of it: a host moving conversations off another system has links,
   * bookmarks, analytics rows and its own foreign keys already pointing at
   * those ids, and an import that minted fresh ones would be a copy rather than
   * a migration. It is also why this is not `createChat` plus a loop of
   * `appendMessage`: that loop can only ever build the tree one legal append at
   * a time, cannot express an inactive branch that was never the live path, and
   * leaves a half-written conversation behind when it fails in the middle.
   *
   * WHAT THE CALLER OWNS: identity, the parent links, the `active` flags, and
   * the tool linkage (`toolCallId` / `toolCalls` / `modelResultJson`), which is
   * persisted VERBATIM so a migrated conversation still replays in an order a
   * provider accepts.
   * WHAT THE STORE OWNS: `orderKey` (creation order, `1..n`), `depth`, and
   * `branchIndex` (per the same sibling rules every append follows). See
   * {@link ImportMessageInput}.
   *
   * VALIDATED IN FULL BEFORE ANY WRITE, all-or-nothing, every failure an
   * `InvalidImportError` carrying `details.reason`:
   * - `duplicate_chat` — the chat id already exists.
   * - `duplicate_message_id` — two messages share an id.
   * - `unknown_parent` / `forward_parent` — a `parentMessageId` naming nothing
   *   in the payload, or naming something later in it. "Creation order" is a
   *   requirement, not a hint: a store cannot assign `depth` to a message whose
   *   parent it has not seen.
   * - `no_active_path` / `broken_active_chain` / `active_leaf_has_child` — the
   *   `active` set is not exactly one chain from a root to a CHILDLESS leaf.
   *   That is the same invariant every other method here maintains, so an
   *   import is held to it too rather than being the one door through which a
   *   chat with two live answers can enter.
   *
   * An EMPTY message list is valid and produces an empty chat. A non-empty list
   * with nothing active is not: a conversation whose active path is empty has
   * no answer to "what is this chat?", and every reader here would report it as
   * blank.
   */
  importConversation(input: ImportConversationInput): Promise<ChatRecord>;
  /**
   * Full-text search over message bodies, best match first.
   *
   * OPTIONAL — a store that cannot index text omits it, and a caller checks for
   * the method rather than catching a "not supported" error. The store
   * conformance suite grades it behind `capabilities.search`.
   *
   * The contract every implementation shares:
   * - The searched text of a message is its string body, or ALL of its text
   *   parts joined with `"\n"` (see `searchTextOf`). Not the first part: a
   *   multimodal message's answer is as likely to be in its third paragraph as
   *   its first, and an index that stopped at part one would silently never
   *   find it.
   * - `internal: true` records and `placeholder: true` records are EXCLUDED.
   *   Neither is chat: one is replay bookkeeping the user never saw, the other
   *   is an answer that has not been written yet.
   * - Hits are NOT restricted to the active path. A message on a branch the
   *   conversation moved away from is still something that was said in this
   *   chat, and it is exactly what a user searching for "where did I see that?"
   *   is trying to find again. A caller that wants only live messages
   *   intersects the hits with `listMessages` itself.
   * - A query that is empty once the store has sanitized it returns `[]` rather
   *   than raising: search boxes emit punctuation, and a 500 from a stray `*`
   *   is a worse answer than no results.
   *
   * EVERYTHING ELSE IS ADAPTER-DEFINED, and deliberately so. The one matching
   * rule this port guarantees is that **a message containing the whole query
   * term is returned, and ranks above messages that match less of it** — that
   * is what the conformance suite pins (`search-conformance.ts`), and it is the
   * most any two honest implementations of "full-text search" can be held to.
   * Beyond it the reference adapters already disagree, in ways a caller must
   * not build on:
   *
   * - **sqlite** is FTS5: the query is tokenized and the tokens are AND-ed, so
   *   `"quartz beacon"` finds a message carrying both words ANYWHERE in it, in
   *   either order, and ranking is bm25.
   * - **memory** is a case-insensitive SUBSTRING scan for the whole trimmed
   *   query, ranked by how many times it occurs, so `"quartz beacon"` finds
   *   only a message with that exact phrase in it.
   *
   * Stemming, prefix and phrase syntax, stop words, ranking function, and how
   * many hits a term must appear in to be worth returning are all the store's
   * own business. A host that needs a specific query language should own the
   * index rather than push its dialect through this port — and a conformance
   * test that pinned one adapter's dialect would be pinning a bug for every
   * other one.
   */
  searchMessages?(
    query: string,
    opts?: SearchMessagesOptions,
  ): Promise<MessageSearchHit[]>;
}

// ---- agent-input-store ----

export type AgentInputKind = "steer" | "follow_up" | "human_request";

export type AgentInputStatus =
  | "pending"
  | "applied"
  | "converted"
  | "answered"
  | "cancelled";

/** What happens to a steer whose target turn ends before it is placed. */
export type AgentInputSettlePolicy = "follow_up" | "cancel";

export interface AgentInputRecord {
  /** Primary key and the caller's idempotency key. */
  inputId: string;
  /** The chat id. */
  sessionId: string;
  kind: AgentInputKind;
  status: AgentInputStatus;
  /** JSON-serializable. */
  content: unknown;
  /** Caller-supplied digest of `content`; how a replay is told from a conflict. */
  contentHash: string;
  /** Store-assigned, strictly increasing insertion order (the FIFO key). */
  order: number;
  targetTaskId?: string;
  /** Steer only. */
  onSettled?: AgentInputSettlePolicy;
  /** Human request: the generation and call an answer must match. */
  attemptId?: string;
  toolCallId?: string;
  expiresAt?: string;
  /** Receipt: where the input landed. */
  appliedTaskId?: string;
  appliedMessageId?: string;
  answerId?: string;
  createdAt: string;
  updatedAt: string;
}

export interface CreateAgentInput {
  inputId: string;
  sessionId: string;
  kind: AgentInputKind;
  content: unknown;
  contentHash: string;
  targetTaskId?: string;
  onSettled?: AgentInputSettlePolicy;
  attemptId?: string;
  toolCallId?: string;
  expiresAt?: string;
}

export interface AgentInputQuery {
  sessionId: string;
  kinds?: readonly AgentInputKind[];
  statuses?: readonly AgentInputStatus[];
  targetTaskId?: string;
}

export interface AgentInputPatch {
  appliedTaskId?: string;
  appliedMessageId?: string;
  answerId?: string;
}

/**
 * Durable inputs that do not become a turn immediately (ADR 0016 §2).
 *
 * Optional on {@link AssistantStore} so it can join the same transaction as
 * tasks and messages without breaking third-party stores.
 */
export interface AgentInputStore {
  /**
   * Idempotent on `inputId`: the same kind, session and `contentHash` returns
   * `{ created: false, record: existing }`; anything differing throws
   * {@link AgentInputConflictError}.
   */
  createInput(
    input: CreateAgentInput,
  ): Promise<{ record: AgentInputRecord; created: boolean }>;
  getInput(inputId: string): Promise<AgentInputRecord | undefined>;
  /** Ordered by `order` ascending. */
  listInputs(query: AgentInputQuery): Promise<AgentInputRecord[]>;
  /**
   * Compare-and-set: throws {@link AgentInputTransitionError} unless the
   * current status is in `from` AND the edge is legal for the record's kind.
   * Writes `patch` and `updatedAt`.
   */
  transitionInput(
    inputId: string,
    from: readonly AgentInputStatus[],
    to: AgentInputStatus,
    patch?: AgentInputPatch,
  ): Promise<AgentInputRecord>;
  /**
   * Remove every input of a session, whatever its status, and return how many
   * rows went. Called inside `ConversationService.deleteChat`'s transaction so
   * a deleted chat leaves no orphaned inputs behind.
   */
  deleteSessionInputs(sessionId: string): Promise<number>;
}

// ---- context-summary-store ----

export interface ContextSummaryRecord {
  summaryId: string;
  /** The chat id. */
  sessionId: string;
  /** Last canonical record covered (inclusive). */
  coversThroughMessageId: string;
  /** First record NOT covered (start of the kept tail). */
  firstKeptMessageId: string;
  summary: string;
  /** sha256 hex over the covered records' (id, role, content, toolCallId), in order. */
  sourceHash: string;
  tokensBefore: number;
  tokensAfter: number;
  providerId?: string;
  model: string;
  summarizerRevision: string;
  details: { readFiles: string[]; modifiedFiles: string[] };
  createdAt: string;
}

/**
 * Derived, versioned summaries of a session's older history (ADR 0017 part 2).
 *
 * A summary never replaces canonical records: it is a cache a turn may use
 * while it still matches the path it was made from, and ignores when it does
 * not (branch switch, fork, edit). Optional on {@link AssistantStore} so
 * third-party stores stay compatible.
 */
export interface ContextSummaryStore {
  /** Idempotent on `summaryId`: a repeat save leaves the first record. */
  saveSummary(record: ContextSummaryRecord): Promise<void>;
  /** Newest first. */
  listSummaries(sessionId: string): Promise<ContextSummaryRecord[]>;
  /** Remove every summary of a session; returns how many rows went. */
  deleteSessionSummaries(sessionId: string): Promise<number>;
}

// ---- outbox-store ----
/**
 * Transactional outbox.
 *
 * A run's events are written to the run log in the same transaction as the state
 * they describe; publishing them to the outside world (SSE, a websocket, a
 * message bus) is a separate, retryable step. Without the outbox the host would
 * have to choose between publishing before the commit (announcing work that may
 * roll back) and publishing after it (losing the announcement on a crash).
 */
export interface OutboxRecord {
  id: string;
  /** Routing key: which stream/queue this belongs to. */
  topic: string;
  /** Correlation key, so a consumer can filter one run's traffic. */
  runId?: string;
  payload: Record<string, unknown>;
  createdAt: string;
  /** Not claimable before this instant (retry backoff). */
  availableAt: string;
  attempts: number;
  publishedAt?: string;
  lastError?: string;
}

export interface OutboxAppendInput {
  id?: string;
  topic: string;
  runId?: string;
  payload: Record<string, unknown>;
  availableAt?: string;
}

export interface OutboxClaimInput {
  limit: number;
  now: Date;
  /** Owner of the claim, so two publishers do not double-send. */
  ownerId?: string;
}

export interface OutboxStore {
  /**
   * Persist a record for publication.
   *
   * `availableAt` is NORMALIZED to a UTC ISO instant
   * (`new Date(x).toISOString()`), because the reference adapters compare it as
   * TEXT: an offset-form string (`…T01:30:00-05:00`) sorts before a `Z` string
   * naming a later instant, so an un-normalized value is claimed hours early.
   * An unparsable value is rejected rather than stored.
   */
  enqueue(input: OutboxAppendInput): Promise<OutboxRecord>;
  /**
   * Claim up to `limit` due records, incrementing their attempt counter.
   *
   * BOUNDED BY ATTEMPTS. A record that has been handed out `maxAttempts` times
   * (adapter option, default 10) is never claimed again: it stays in the table
   * with its `attempts` and `lastError` as an inspectable dead letter, rather
   * than being redelivered forever to a consumer that cannot accept it. There
   * is no separate "dead" flag — `attempts` IS the record of how many times the
   * publisher tried, and a cap read off it needs no schema of its own.
   */
  claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]>;
  /** MUST reject an unknown id with {@link RecordNotFoundError}. */
  markPublished(id: string, at: Date): Promise<void>;
  /**
   * Record the failure and schedule the retry. At the attempt cap the retry
   * schedule no longer matters — `claimBatch` will not take the record again —
   * but the error is still recorded, because that string is the only diagnosis
   * of why publication was abandoned.
   *
   * MUST reject an unknown id with {@link RecordNotFoundError}.
   */
  markFailed(id: string, error: string, retryAt: Date): Promise<void>;
  /**
   * Delete records that can never be claimed again and are older than
   * `before` — published ones (compared on `publishedAt`) and attempt-exhausted
   * ones (compared on `createdAt`). Returns how many rows were removed.
   *
   * RETENTION IS A CALLER'S DECISION, which is why this takes an instant
   * instead of running itself on a timer: how long a published event stays
   * readable is a product question (an audit trail, a debugging window), and a
   * store that swept on its own would answer it for every host. Nothing
   * claimable is ever removed, whatever `before` says — a pruner that could
   * delete work still waiting to be published would be a data-loss button.
   */
  prune(before: Date): Promise<number>;
}

// ---- provider-continuation-store ----

export type ProviderContinuationScope = AiProviderContinuation["scope"];

/** Trusted provider state; never part of message metadata, events or REST DTOs. */
export interface ProviderContinuationRecord {
  runId: string;
  anchorMessageId: string;
  state: AiProviderContinuation;
}

/** Optional private persistence for lossless, branch-bound provider continuation. */
export interface ProviderContinuationStore {
  /** Insert once, or require an exact match with the original run identity. */
  bindRun(
    runId: string,
    scope: ProviderContinuationScope,
    fence: RunWriteFence,
  ): Promise<void>;
  getRunScope(runId: string): Promise<ProviderContinuationScope | null>;
  /** One latest state per run, persisted before its tool calls are dispatched. */
  put(
    input: ProviderContinuationRecord,
    fence: RunWriteFence,
  ): Promise<ProviderContinuationRecord>;
  /** Null before any successful save; missing previously saved state throws provider_continuation_state_missing. */
  getByRun(runId: string): Promise<ProviderContinuationRecord | null>;
  /** Input-anchor order, then run ID; at most 1024 anchor IDs. */
  findByAnchors(
    chatId: string,
    anchorMessageIds: readonly string[],
  ): Promise<ProviderContinuationRecord[]>;
}

// ---- provider-store ----

/**
 * Persistence for configured providers, their model catalogues, and their
 * probed capabilities.
 *
 * Capabilities are stored rather than probed per run because probing costs a
 * request and its answer is stable for hours; a run that had to discover
 * "does this endpoint do tool calling?" every time would pay for it on every
 * turn.
 *
 * Secrets: `AiProviderConfig.apiKey` is part of the DTO, but a host that keeps
 * keys out of its main database stores a reference in `metadata` and resolves it
 * through {@link SecretStore} instead — see `secret-store.ts`.
 */
export interface ProviderStore {
  listProviders(): Promise<AiProviderConfig[]>;
  getProvider(providerId: string): Promise<AiProviderConfig | null>;
  upsertProvider(config: AiProviderConfig): Promise<AiProviderConfig>;
  deleteProvider(providerId: string): Promise<void>;

  listModels(providerId: string): Promise<AiProviderModel[]>;
  /** Replace the catalogue wholesale — a refresh is a snapshot, not a merge. */
  replaceModels(providerId: string, models: AiProviderModel[]): Promise<void>;

  getCapabilities(providerId: string): Promise<AiProviderCapabilities | null>;
  saveCapabilities(
    providerId: string,
    capabilities: AiProviderCapabilities,
  ): Promise<void>;
}

// ---- settings-store ----

/**
 * Assistant-wide settings. One row: these are the knobs a host exposes in a
 * settings pane, not per-chat state.
 */
export interface AssistantSettings {
  defaultProviderId?: string;
  defaultModel?: string;
  /** Drives `resolveToolLimits` — how much tool output may enter the context. */
  contextSizePreference: AiContextSizePreference;
  /** Default confirmation posture for writes; see {@link WritePolicyMode}. */
  writePolicyMode: WritePolicyMode;
  /** Expose raw tool payloads to the model instead of slim envelopes. */
  allowRawToolData: boolean;
  maxToolIterations?: number;
  /**
   * Manual override for whether a turn is handed tools at all. Default `auto`.
   *
   * - `auto` — follow the provider's probed capabilities: a provider recorded as
   *   `toolCalling: false` gets no tools, because handing them over is the exact
   *   request shape that fails.
   * - `on` — stage tools even when the probe says unsupported. This is the whole
   *   reason the knob exists: probing is a heuristic against someone else's
   *   server, and a wrong `false` otherwise leaves a perfectly capable model
   *   permanently toolless with no way to say so.
   * - `off` — stage no tools at all, whatever the provider can do. A chat-only
   *   mode, and the cheapest way to take a misbehaving tool loop out of the
   *   picture without unwiring contributors.
   */
  toolCalling?: ToolCallingMode;
  metadata: Record<string, unknown>;
}

/** See {@link AssistantSettings.toolCalling}. */
export type ToolCallingMode = "auto" | "on" | "off";

export interface SettingsStore {
  getSettings(): Promise<AssistantSettings>;
  /** Partial update; returns the settings as they now stand. */
  updateSettings(patch: Partial<AssistantSettings>): Promise<AssistantSettings>;
}

// ---- task-store ----

/**
 * Lifecycle of a durable task.
 *
 * `queued` is not a formality: a submitted task is persisted and acknowledged
 * before any worker touches it, so a crash between "the host accepted the work"
 * and "the executor started" loses nothing. `waiting_approval` exists because a
 * task that staged a write and is waiting on a human is neither running nor
 * finished, and calling it either would make the queue's recovery pass do the
 * wrong thing.
 *
 * `waiting_approval` is a PARK (ADR 0016 §5): the chat-turn executor produces it
 * when a session turn suspends for human input. A parked task holds no lease and
 * all its attempts are settled (the last one `completed`); answering the request
 * re-admits it through `waiting_approval → queued`. A staged write, by contrast,
 * still returns `pending` to the model and the turn completes.
 */
export type TaskStatus =
  | "queued"
  | "running"
  | "interrupted"
  | "waiting_approval"
  | "completed"
  | "failed"
  | "cancelled";

/**
 * Lifecycle of ONE attempt at a task: `running` → `completed` | `failed` |
 * `abandoned` | `cancelled`, and no edge out of a terminal one. `abandoned` is
 * the crash outcome: the lease expired with the attempt still marked running,
 * so recovery ends it that way rather than claiming knowledge of success or
 * failure it does not have.
 *
 * The terminal statuses are terminal in the store as well as on paper — see
 * {@link TaskStore.endAttempt} for what a second `endAttempt` does.
 */
export type AttemptStatus =
  | "running"
  | "completed"
  | "failed"
  | "abandoned"
  | "cancelled";

/**
 * One dependency as the queue reads it while gating a claim — the two facts
 * that decide the gate, and nothing else, so a store can answer from a narrow
 * projection instead of loading whole records.
 */
export interface TaskDependencyState {
  taskId: string;
  /** null when the row is gone. */
  status: TaskStatus | null;
  /** `deadLetteredAt` is set — the queue gave up on it, whatever its status says. */
  deadLettered: boolean;
}

/**
 * What a queued task's dependencies say about it right now: claim it, skip it,
 * or end it here.
 */
export type TaskDependencyVerdict =
  | { kind: "ready" }
  /** Some dependency is still in flight; try again on a later claim. */
  | { kind: "blocked" }
  /**
   * No dependency can ever complete now. The claim transitions the dependent to
   * `to` INSTEAD of claiming it — `error` is set for `failed` and absent for
   * `cancelled`, because a cancellation is not a failure and recording one as
   * an error would make every "why did this fail?" dashboard lie.
   */
  | { kind: "settle"; to: "failed" | "cancelled"; error?: string };

export interface TaskRecord {
  taskId: string;
  /**
   * What kind of work this is, and therefore which executor runs it. Host-chosen
   * (see `CHAT_TURN_TASK_KIND` and the reserved prefixes documented beside it);
   * the store treats it as an opaque string it filters and returns.
   */
  kind: string;
  /**
   * Serialization key. Tasks sharing a scope never execute concurrently, so two
   * turns cannot write the same document at once. Usually the chat id, but a
   * host that writes a shared document scopes on the document instead.
   */
  scopeId: string;
  status: TaskStatus;
  /** Higher tasks first; combined with age when the queue claims work. */
  priority: number;
  enqueuedAt: string;
  /** Not claimable before this instant (backoff / scheduled work). */
  availableAt: string;
  startedAt?: string;
  finishedAt?: string;
  /**
   * What to run, in whatever shape the executor for this `kind` understands —
   * for a chat turn: the chat, model, provider, and the message ids the turn
   * owns.
   *
   * MUST be serializable: a durable store round-trips it through its own
   * encoding, and the reference adapters copy it on the way in and out so a
   * caller that keeps mutating the object it submitted cannot edit a task
   * already queued.
   */
  payload: Record<string, unknown>;
  /**
   * The task that spawned this one, when an executor fanned work out (see
   * `TaskExecutionContext.spawnChild`).
   *
   * LINEAGE ONLY — it is not a dependency and not a lifecycle coupling. A child
   * runs the moment the queue can claim it, whether or not the parent is still
   * running, because the parent usually spawns children precisely so they can
   * proceed without it. What lineage buys is the two questions a fan-out host
   * actually asks: "what did this task set off?"
   * ({@link TaskStore.listChildren}) and "cancel this whole branch"
   * (`TaskService.cancelTask`). A parent that must WAIT for its children
   * expresses that with a third task that `dependsOn` them — the continuation
   * pattern — not by conflating the two edges.
   */
  parentTaskId?: string;
  /**
   * Task ids that must reach `completed` before this task may be claimed.
   *
   * IMMUTABLE AFTER CREATE, and every id must already exist when the dependent
   * is written ({@link UnknownDependencyError} otherwise). Those two rules
   * together are what make the graph acyclic by construction: an edge can only
   * point at an older row, so there is no order of writes that produces a
   * cycle, and nothing can add one later. A queue that can deadlock on
   * committed data is a queue that needs a human at 3am; this one cannot.
   *
   * The gate is enforced in {@link TaskStore.claimNext} — dependency state is
   * queue semantics, not orchestration — and a dependency that ends badly
   * settles the dependent rather than parking it forever. See
   * {@link evaluateTaskDependencies}.
   */
  dependsOn?: string[];
  /**
   * Last-known progress of a running task: an OVERWRITTEN snapshot ("42 of 900
   * files"), never an append.
   *
   * Deliberately not an event. The event log is the durable, ordered,
   * replayable record of what happened and it is what a UI replays after a
   * crash — writing a heartbeat percentage into it every second would bloat
   * that log with entries nobody will ever replay, and force every consumer to
   * fold thousands of superseded numbers to learn one current value. Progress
   * is the opposite kind of data: only the latest matters, and losing an
   * intermediate value costs nothing. See {@link TaskStore.updateProgress}.
   */
  progress?: Record<string, unknown>;
  error?: string;
  attemptCount: number;
  /**
   * Attempts that died without a clean end (`abandoned`).
   *
   * NOT the dead-letter trigger — `attemptCount` is (the runner compares it
   * with its own `maxAttempts`). This is the diagnosis that travels with the
   * dead letter: "three attempts, and all three were abandoned" is a crashing
   * worker, while "three attempts, none abandoned" is work that fails cleanly.
   *
   * THE STORE OWNS IT: {@link TaskStore.endAttempt} increments it by one when
   * an attempt ends `abandoned`, in the same write as the attempt row, and
   * leaves it alone for every other status. A caller cannot get that right —
   * it reads the count, then writes count + 1 on a later transition, and both
   * a crash in the gap and a second recoverer reading the same value lose a
   * death that did happen. {@link TaskPatch.poisonCount} remains for a host
   * that must overwrite the number outright.
   */
  poisonCount: number;
  deadLetteredAt?: string;
  deadLetterReason?: string;
}

export interface CreateTaskInput {
  taskId: string;
  kind: string;
  scopeId: string;
  payload: Record<string, unknown>;
  priority?: number;
  /**
   * Not claimable before this instant. NORMALIZED by the store to a UTC ISO
   * string (`new Date(x).toISOString()`) — see {@link TaskPatch.availableAt}.
   */
  availableAt?: string;
  /** Lineage — see {@link TaskRecord.parentTaskId}. Must already exist. */
  parentTaskId?: string;
  /** Gate — see {@link TaskRecord.dependsOn}. Every id must already exist. */
  dependsOn?: string[];
  /**
   * Refuse this create — {@link ChatBusyError}, code `chat_busy` — when ANY
   * task in `scopeId` is still unfinished (not one of
   * {@link TERMINAL_TASK_STATUSES}). Default `false`: the queue's own model is
   * that a scope SERIALIZES its tasks rather than admitting one at a time.
   *
   * `queued` counts as unfinished here, unlike in `deleteByScope`'s busy check,
   * and that difference is the point. A delete refuses on work that is
   * happening; this refuses on work that is going to happen, because the caller
   * asking for exclusivity is asking "is this scope free RIGHT NOW", and a
   * queued turn is a turn whose answer is already promised.
   *
   * THE CHECK MUST BE THE FIRST STATEMENT OF THE SAME TRANSACTION AS THE
   * INSERT, with no `await` between them. Two submits racing is precisely the
   * case this exists for, so a check the caller makes before calling — where
   * the read and the write are separated by an await — is not the same
   * guarantee and cannot be substituted for this one.
   *
   * A DUPLICATE `taskId` STILL WINS. When the id already exists, this raises
   * {@link DuplicateTaskError} exactly as it would without the flag, and the
   * scope check never runs: a redelivery of a submit whose task is still
   * running is a retry to be answered from the existing record, not a second
   * turn to refuse — and reporting it as `chat_busy` would make an idempotent
   * caller retry forever.
   */
  exclusiveScope?: boolean;
}

/** Fields a transition may write alongside the status change, atomically. */
export interface TaskPatch {
  startedAt?: string;
  finishedAt?: string;
  error?: string;
  /**
   * Reschedule the task's earliest claim.
   *
   * NORMALIZED to a UTC ISO instant before it is stored, and an unparsable
   * value is REJECTED rather than written. Both matter because a store compares
   * this field as TEXT: `2026-01-01T01:30:00-05:00` names 06:30Z but sorts
   * before `2026-01-01T02:00:00.000Z`, so an un-normalized offset-form backoff
   * is claimed hours before it is due — and one adapter parsing it as a date
   * while another compares the raw string is a divergence no conformance case
   * would see unless it used an offset.
   */
  availableAt?: string;
  priority?: number;
  /**
   * OVERWRITE the poison count, for a host with a reason to.
   *
   * Not how the count is normally written: {@link TaskStore.endAttempt}
   * increments it itself when an attempt ends `abandoned`, so a transition that
   * omits this field keeps whatever the store has counted. Setting it here
   * replaces that number rather than adding to it.
   */
  poisonCount?: number;
  payload?: Record<string, unknown>;
}

export interface AttemptRecord {
  attemptId: string;
  taskId: string;
  /** 1-based; `attemptCount` at the time the attempt was created. */
  attemptNumber: number;
  status: AttemptStatus;
  ownerId: string;
  startedAt: string;
  endedAt?: string;
  error?: string;
}

export interface CreateAttemptInput {
  attemptId: string;
  taskId: string;
  ownerId: string;
}

export interface EndAttemptInput {
  attemptId: string;
  status: Exclude<AttemptStatus, "running">;
  error?: string;
  /**
   * Proof of ownership — see {@link FencedWriteOptions}. Absent means the
   * write is not fenced, which is what recovery needs: it ends the attempt of
   * a lease that has already been expired and deleted, so there is no token
   * left to prove anything with.
   */
  leaseToken?: string;
}

/**
 * Exclusive, expiring ownership of a task.
 *
 * `leaseToken` proves "I am the current owner" on every write. `fencingToken` is
 * monotonic across ALL leases, so a resumed-from-the-dead worker holding an old
 * lease can be rejected by comparison even if it somehow re-acquires: the
 * classic fix for the process that paused for a minute and woke up believing it
 * still owned the task.
 */
export interface Lease {
  taskId: string;
  attemptId: string;
  ownerId: string;
  leaseToken: string;
  fencingToken: number;
  expiresAt: string;
}

export interface AcquireLeaseInput {
  taskId: string;
  attemptId: string;
  ownerId: string;
  ttlMs: number;
}

/**
 * Ownership proof for the writes that END a task or an attempt.
 *
 * `appendEvents` and `updateProgress` have always demanded a `leaseToken`,
 * because two attempts interleaving into one event stream is obviously wrong.
 * The TERMINAL writes did not, and that was the bigger hole: an attempt whose
 * lease expired mid-tool-call (recovery has since started attempt 2) would
 * still transition the task `completed` and end its own attempt, burying the
 * live attempt's verdict under a zombie's. A runner cannot close that from
 * outside — its `renewLease` pre-check and the write it guards are separated by
 * awaits — so the proof has to be checked INSIDE the adapter's own
 * transaction, next to the write.
 *
 * OPTIONAL, and deliberately so. A host that lands a task from outside any
 * lease (a cancel from an HTTP handler, a boot-time reaper, `TaskService`) has
 * no token to offer, and refusing those writes would break the very recovery
 * paths that repair a crashed run. Absent means "unfenced, as before"; present
 * means "verify or refuse".
 */
export interface FencedWriteOptions {
  /**
   * The writer's current lease token. When given, the store MUST verify inside
   * the same transaction as the write that this token names the task's CURRENT
   * lease, and throw {@link LeaseLostError} otherwise.
   */
  leaseToken?: string;
}

export interface AppendEventsOptions {
  /** The writer's proof of ownership. Stale token ⇒ `LeaseLostError`. */
  leaseToken: string;
}

export interface UpdateProgressOptions {
  /**
   * The writer's proof of ownership — the SAME check `appendEvents` makes, for
   * the same reason: a zombie worker whose lease was taken over must not
   * overwrite the live attempt's progress with a snapshot from the run nobody
   * is watching any more. Stale token ⇒ `LeaseLostError`.
   */
  leaseToken: string;
}

export interface ListEventsOptions {
  afterSeq?: number;
  limit?: number;
}

export interface ClaimNextInput {
  ownerId: string;
  now: Date;
  /** Scopes with a task already executing; the claim must skip them. */
  scopesBusy: string[];
  /**
   * When present, only tasks whose `kind` is in this list may be claimed.
   *
   * A deployment can run several worker processes that register DIFFERENT
   * executor sets (the GPU box takes `index.embed`, the web box takes
   * `chat.turn`). Without the filter a worker claims work it cannot execute,
   * and the task fails on `ExecutorNotFoundError` instead of waiting for the
   * process that could have run it. Absent means "any kind".
   */
  kinds?: string[];
}

export interface ClaimedTask {
  task: TaskRecord;
  attempt: AttemptRecord;
  lease: Lease;
}

/**
 * Durable task state: lifecycle, attempts, leases, and the event log.
 *
 * The event log is the product of a task — everything a UI replays and
 * everything a crash recovery reads — so its two invariants are contractual, not
 * advisory (see {@link TaskStore.appendEvents}).
 */
export interface TaskStore {
  /**
   * Persist a new `queued` task.
   *
   * MUST reject a `taskId` that already exists with {@link DuplicateTaskError},
   * never silently overwrite: the id is the caller's idempotency key (a retried
   * HTTP submit, a replayed message), and quietly replacing the row would
   * discard an in-flight task's payload, status, and attempt history while
   * leaving its events behind.
   *
   * MUST also reject with {@link UnknownDependencyError} when `parentTaskId` or
   * any `dependsOn` id is not already in the store, and when `dependsOn`
   * contains the task's own id. That check is the acyclicity proof — see
   * {@link TaskRecord.dependsOn} — so a store that skipped it would accept a
   * graph its own `claimNext` can never drain.
   *
   * And with {@link CreateTaskInput.exclusiveScope}, MUST reject with
   * {@link ChatBusyError} when the scope already holds an unfinished task,
   * checked ATOMICALLY with the insert — see that field.
   */
  createTask(input: CreateTaskInput): Promise<TaskRecord>;
  getTask(taskId: string): Promise<TaskRecord | null>;
  /** Park pre-boot queued work for explicit user resume. Optional for custom adapters. */
  interruptQueued?(before: string): Promise<number>;

  /** Atomically park queued/running work, close running attempts as cancelled, and revoke its lease.
   * Terminal outcomes and prior interruptions are unchanged. Events and payload remain intact. */
  interruptTask?(
    taskId: string,
    reason: string,
    opts?: FencedWriteOptions,
  ): Promise<boolean>;

  /**
   * Tasks whose `parentTaskId` is `taskId` — one level, not the whole subtree.
   *
   * One level because the caller that needs the subtree
   * (`TaskService.cancelTask`) is already walking breadth-first and can ask
   * again; a store-side recursive walk would make every adapter implement a
   * traversal to serve one consumer, and would return a snapshot that is stale
   * by the time it is read anyway.
   */
  listChildren(taskId: string): Promise<TaskRecord[]>;

  /**
   * Every task in a scope, whatever its status.
   *
   * Deliberately the NARROWEST query that answers the one question a caller
   * outside this port actually has — "is anything still live in this chat?",
   * which `ConversationService.deleteChat` asks before it deletes anything. A
   * general `listTasks(filter)` would have to grow statuses, kinds, ordering
   * and paging, and every adapter would owe an implementation of all of it to
   * serve one consumer that wants a handful of rows from one indexed lookup.
   *
   * No paging, for the same reason: a scope is a serialization key (usually one
   * chat), so its task count is bounded by how much work that one conversation
   * has ever queued, and a caller that must inspect all of them cannot use a
   * page anyway.
   */
  listByScope(scopeId: string): Promise<TaskRecord[]>;

  /**
   * Delete every task in a scope — with its attempts, its lease and its event
   * log — and return how many TASKS were removed.
   *
   * REFUSES with {@link ChatBusyError} (`chat_busy`) when ANY task in the scope
   * is `running` or `waiting_approval`, and then deletes nothing at all. THE
   * STORE OWNS THIS GUARANTEE, and an implementation MUST make the check and
   * the deletes ONE synchronous statement or transaction with no `await`
   * between them.
   *
   * WHY THE STORE AND NOT THE CALLER. `ConversationService.deleteChat` still
   * checks first — it wants to refuse before it has deleted the conversation —
   * but its check cannot be the guarantee. It reads the scope and then deletes
   * inside one async transaction, with an `await` in between, and a
   * check-then-act invariant that spans an `await` is not atomic no matter how
   * well-wrapped the transaction is. The reference adapters now QUEUE an
   * unrelated caller's write behind an open transaction rather than letting it
   * flatten in (that is how a concurrent `claimNext` used to move a task
   * `queued → running` inside the gap), but that is an adapter's courtesy, not
   * something this port can promise of every store. The hazard is a CLASS: an
   * invariant of that shape has to be enforced by a single synchronous
   * statement or transaction inside the adapter — which is what this method
   * does.
   *
   * `queued` is deliberately NOT live: nothing has been spent on it, and
   * refusing on one would make a chat undeletable for as long as anything sat
   * merely enqueued behind it. Force-cancelling what IS live is a different
   * operation with different consequences (it ends a provider call mid-flight),
   * so this method refuses instead of inventing it; the caller cancels
   * explicitly and deletes after.
   *
   * The event log goes with the tasks: it is the record of what those attempts
   * did, and orphaning it would leave rows nothing can ever name again.
   */
  deleteByScope(scopeId: string): Promise<number>;

  /**
   * Compare-and-set the task's status.
   *
   * MUST reject with {@link InvalidTaskTransitionError} when the current status
   * is not in `from` (someone else moved it first — a lost race, not a retryable
   * hiccup) or when `from → to` is not in {@link TASK_TRANSITIONS}. Passing the
   * expected `from` set rather than blind-writing is what makes cancel-vs-finish
   * races resolve to exactly one winner.
   *
   * With `opts.leaseToken`, the CAS is fenced as well: the store MUST verify
   * ownership inside the same transaction and reject a stale token with
   * {@link LeaseLostError} — see {@link FencedWriteOptions}.
   */
  transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord>;

  createAttempt(input: CreateAttemptInput): Promise<AttemptRecord>;
  /**
   * Close an attempt with its outcome.
   *
   * FIRST TERMINAL WINS. An attempt that is no longer `running` is returned
   * UNCHANGED — same status, same `endedAt`, same `error` — and nothing is
   * written. {@link AttemptStatus} has no edge out of a terminal state, and the
   * second report is by construction the less informed one: it comes from a
   * recovery pass acting on an expired lease, or from a caller retrying after a
   * failure that happened AFTER the attempt row already landed. A store that
   * overwrote would let a clean `completed` be restated as `abandoned` and
   * counted as a crash. This is also why re-reporting the same death is free
   * rather than a special case.
   *
   * A `running` → `abandoned` end MUST also increment the task's
   * {@link TaskRecord.poisonCount} by one, atomically with the attempt write —
   * the attempt ending that way IS the poison event, and a count kept by
   * callers loses deaths (see that field). Every other edge leaves it alone.
   *
   * With {@link EndAttemptInput.leaseToken}, fenced the same way
   * {@link transitionTask} is: the token must name the CURRENT lease of the
   * attempt's task, or the write is refused with {@link LeaseLostError}. The
   * fence is checked BEFORE the terminal short-circuit — "may you write here?"
   * is a different question from "is there anything to write?", and a caller
   * whose lease moved should learn that it did.
   */
  endAttempt(input: EndAttemptInput): Promise<AttemptRecord>;

  acquireLease(input: AcquireLeaseInput): Promise<Lease>;
  /** MUST throw {@link LeaseLostError} when the token is no longer current. */
  renewLease(leaseToken: string, ttlMs: number): Promise<Lease>;
  releaseLease(leaseToken: string): Promise<void>;
  /** Ends leases past their expiry and returns them, so recovery can act. */
  expireStaleLeases(now: Date): Promise<Lease[]>;

  /**
   * Append events to the task's log.
   *
   * Contract — an implementation MUST:
   * - reject a stale `leaseToken` with {@link LeaseLostError}, so a zombie
   *   worker cannot interleave its events with the live attempt's;
   * - reject non-monotonic `seq` with {@link SeqConflictError} — a repeat or a
   *   gap breaks the consumer's only means of detecting a dropped event.
   *
   * It MUST NOT re-stamp `seq`: the emitter owns numbering (core's
   * `RunChatInput.firstSeq` inside a chat pass, `createTaskEventWriter`
   * elsewhere), so a store that renumbered would desync the ids already streamed
   * to a client.
   *
   * A REPEATED `eventId` is rejected with {@link SeqConflictError} too. The
   * guaranteed scope of that uniqueness is ONE TASK — that is all a consumer
   * deduping a replayed stream needs, and all a store can promise cheaply. A
   * store MAY enforce it more widely (the sqlite adapter's `event_id` index is
   * UNIQUE across the whole table, so a collision between two tasks fails there
   * and passes on the memory adapter); ids come from `IdGenerator.eventId`, so
   * a cross-task collision means a broken generator either way.
   *
   * The element type is the task-kind-agnostic {@link TaskEventEnvelope}: the
   * store orders by `seq` and dedups by `eventId` and reads nothing else, so one
   * log holds a chat turn's `AiRunEvent`s and another kind's vocabulary without
   * knowing either.
   */
  appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void>;

  listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]>;

  /**
   * Overwrite the task's {@link TaskRecord.progress} snapshot.
   *
   * REPLACES rather than merges: the writer owns the whole shape, and a merge
   * would make a field that disappeared from one snapshot linger from the last.
   * MUST reject a stale `leaseToken` with {@link LeaseLostError}.
   *
   * NEVER touches the event log, and emits nothing. Progress is state, not
   * history — see {@link TaskRecord.progress}.
   */
  updateProgress(
    taskId: string,
    progress: Record<string, unknown>,
    opts: UpdateProgressOptions,
  ): Promise<TaskRecord>;

  /**
   * The `seq` the next appended event must carry (0 for an empty task). A retry
   * passes it to `runChat` as `firstSeq` so several provider passes produce ONE
   * unbroken sequence under one task id.
   */
  nextSeq(taskId: string): Promise<number>;

  /**
   * Atomically claim the highest-priority claimable task whose scope is free (and
   * whose kind passes `kinds`, when given), creating its attempt and lease.
   * Returns null when there is nothing to do.
   *
   * DEPENDENCIES ARE PART OF CLAIMABILITY, and this is where they are enforced:
   * a task with an unfinished `dependsOn` entry is skipped, and the claim moves
   * on to the next candidate in the same call.
   *
   * A candidate whose dependency ended BADLY is settled here too — transitioned
   * `failed` (with `dependency_failed: <depTaskId>`) or `cancelled` per
   * {@link evaluateTaskDependencies} — instead of being claimed, and the scan
   * continues past it. Doing that lazily, on the claim path, is deliberate:
   * nothing is ever re-enqueued, no background reaper has to exist, and the
   * store stays the single truth about what is runnable. A chain settles over
   * successive claims, each sweep resolving what the previous one unblocked.
   */
  claimNext(input: ClaimNextInput): Promise<ClaimedTask | null>;

  /**
   * Stamp the task as poisoned — the row that says "stop feeding this work".
   *
   * Fenced by `opts.leaseToken` on the same terms as {@link transitionTask}:
   * present, the token must name the task's current lease.
   */
  markDeadLettered(
    taskId: string,
    reason: string,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord>;
}

// ---- tool-execution-store ----

/** An admission is durable before invoking a tool, even if its body never starts. */
export interface ToolExecutionRecord {
  executionId: string;
  runId: string;
  toolCallId: string;
  toolName: string;
  argsSha256: string;
  contextSha256: string;
  attemptId: string;
  approvalRequestId?: string;
  approvalGeneration?: string;
  /** `resolved`: an operator closed an uncertain receipt without a result. */
  status: "admitted" | "completed" | "resolved";
  result?: AiToolResult<unknown>;
  /** Audit of an explicit operator resolution; never written by a run. */
  resolution?: ToolExecutionResolution;
}

export interface ToolExecutionResolution {
  outcome: "completed" | "not_started" | "acknowledged";
  note?: string;
  resolvedAt: number;
  resolvedBy?: string;
}

export type ToolExecutionAdmission = Omit<
  ToolExecutionRecord,
  "status" | "result" | "resolution"
>;

/** Receipts are separate from conversation history and process quarantine. */
export interface ToolExecutionStore {
  /** Atomic, fenced insert. An existing execution or approval returns false. */
  admit(input: ToolExecutionAdmission, fence: RunWriteFence): Promise<boolean>;
  /** Persist the result before delivery to the core loop or UI. */
  complete(
    executionId: string,
    result: AiToolResult<unknown>,
    fence: RunWriteFence,
  ): Promise<void>;
  listByRun(runId: string): Promise<ToolExecutionRecord[]>;
}

// ---- assistant-store ----

/**
 * The host's persistence, as one aggregate.
 *
 * The six stores are grouped rather than injected separately because the
 * operations that matter span them — submitting a turn writes two messages AND a
 * task; finishing one writes events AND a message AND a status — and those writes
 * must land together or not at all. A single aggregate with one
 * {@link AssistantStore.transaction} is what makes that expressible; six
 * independent stores would leave every consumer to invent its own atomicity.
 */
export interface AssistantStore {
  conversations: ConversationStore;
  tasks: TaskStore;
  proposals: ProposalStore;
  /** Durable steer, follow-up and human-input records; optional so existing host stores remain compatible. */
  inputs?: AgentInputStore;
  /** Derived context summaries (ADR 0017 part 2); optional so existing host stores remain compatible. */
  summaries?: ContextSummaryStore;
  providers: ProviderStore;
  settings: SettingsStore;
  outbox: OutboxStore;
  /** Trusted provider state; optional so existing host stores remain compatible. */
  continuations?: ProviderContinuationStore;
  /** Durable mutation admission and approval consumption; required for mutating turns. */
  toolExecutions?: ToolExecutionStore;

  /**
   * Run `fn` in a transaction, handing it a store view scoped to that
   * transaction. Everything `fn` writes through `tx` commits together; a throw
   * rolls all of it back.
   *
   * Implementations need not support nesting: a host that cannot nest should
   * pass the ambient transaction through rather than opening a second one.
   *
   * CALLERS ARE SERIALIZED PER STORE HANDLE. A second `transaction()`, and a
   * worker's `claimNext`, issued while one is open WAIT for it and then run as
   * a unit of their own, so one caller's rollback can only ever discard that
   * caller's writes. Calls the callback makes through the `tx` it is handed
   * JOIN that transaction, and a nested `tx.transaction(...)` flattens into it
   * rather than nesting.
   *
   * THE COROLLARY IS THAT A CALLBACK MUST WORK THROUGH `tx`. A call issued on
   * the ROOT store from inside a callback is, by construction,
   * indistinguishable from an unrelated caller's: it waits for a transaction
   * that cannot finish until the callback returns. That wait is bounded, so it
   * ends in an `AgentKitHostError` with code `transaction_gate_timeout`
   * (`TransactionGateTimeoutError`) rather than a hang — but the fix is always
   * to use `tx`, never to raise the budget. Keep foreign async work out of the
   * callback entirely: do the awaiting outside and pass the results in.
   *
   * ISOLATION CAVEAT: what `transaction()` promises is atomicity and the
   * serialization above, NOT snapshot isolation. Most reads from other callers still
   * see the store mid-transaction. Durable task status, event, and projected message reads must
   * wait for commit or rollback because transports publish their results. An
   * adapter may let an ordinary single-call WRITE from another caller queue
   * behind an open transaction — `SqliteAssistantStore` does exactly that, so
   * such a write is delayed rather than joined and rolled back.
   *
   * Both reference adapters now answer the four questions above the same way:
   * `MemoryAssistantStore` has no rollback (it declares
   * `capabilities.atomicTransactions: false`) and queues task mutations,
   * including lease changes. Other ordinary writes remain immediate. It
   * serializes, flattens and times out exactly as
   * `SqliteAssistantStore` does. The shared conformance suite in
   * `@agentkit/testing` pins that for any adapter a host writes later.
   */
  transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T>;
}
