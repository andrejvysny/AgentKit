/**
 * bun:sqlite-backed, complete {@link AssistantStore} — the durable reference
 * adapter. `bun:sqlite` is fully synchronous; every port method here is
 * `async` only because the port interfaces are, so a host can swap this for a
 * network-backed store without changing call sites.
 *
 * Fencing/lease enforcement pattern: a write guarded by a `leaseToken` first
 * reads the task's current lease inside the same transaction, then performs
 * its INSERT/UPDATE, and rejects on mismatch with {@link LeaseLostError} —
 * see {@link SqliteTaskStore.appendEvents}. Compare-and-set operations
 * (`transitionTask`, `ProposalStore.transition`) use a guarded
 * `UPDATE ... WHERE id = ? AND status = ?` and check the driver's reported
 * `changes` count as the backstop against a race the initial SELECT could not
 * see — see {@link SqliteConnection}.
 */
import { Database } from "bun:sqlite";
import type { Changes } from "bun:sqlite";
import type {
  AiContentPart,
  AiMessageContent,
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
  AiToolCall,
  TaskEventEnvelope,
} from "@agentkit/contracts";
import {
  activationSetOf,
  activePathOf,
  assertAppendActivation,
  assertListMessagesCursors,
  AgentKitHostError,
  ChatBusyError,
  DEFAULT_SEARCH_LIMIT,
  DuplicateActionIdError,
  DuplicateTaskError,
  InvalidImportError,
  InvalidProposalTransitionError,
  InvalidTaskTransitionError,
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
  planForkedMessages,
  planImportedMessages,
  SEARCH_MATCH_END,
  SEARCH_MATCH_START,
  SEARCH_SNIPPET_ELLIPSIS,
  type AppendEventsOptions,
  type AppendMessageInput,
  type ApplyOutcome,
  type AssistantSettings,
  type AssistantStore,
  type AttemptRecord,
  type AttemptStatus,
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
  type FencedWriteOptions,
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
  type ProposalDecision,
  type ProposalPatch,
  type ProposalRecord,
  type ProposalStatus,
  type ProposalStore,
  type ProviderStore,
  type RiskLevel,
  type SettingsStore,
  type TaskDependencyState,
  type TaskPatch,
  type TaskRecord,
  type TaskStatus,
  type TaskStore,
  type ToolCallingMode,
  type UpdateMessagePatch,
  type UpdateProgressOptions,
  type WritePolicyMode,
  resolveTaskAging,
  type ResolvedTaskAging,
  type TaskAgingOptions,
} from "@agentkit/host";
import { SCHEMA_V7, SCHEMA_VERSION } from "./schema.js";

const DEFAULT_LEASE_TTL_MS = 30_000;
/** Mirrors the `settings.tool_calling_mode` DDL default. */
const DEFAULT_TOOL_CALLING_MODE: ToolCallingMode = "auto";
/**
 * How long a transaction waits for another connection's write lock.
 *
 * Generous on purpose: the cost of waiting is latency, and the cost of not
 * waiting is a raw `SQLITE_BUSY` surfacing out of a port method that documents
 * no such failure. See the multi-handle section on {@link SqliteConnection}.
 */
const DEFAULT_BUSY_TIMEOUT_MS = 5_000;
const DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS = 30_000;
/**
 * How many times one outbox record may be handed to a publisher before the
 * queue stops offering it. Ten is generous for a transient consumer outage
 * (with the caller's own backoff between them) and short of "forever", which is
 * what an uncapped outbox meant: a payload no consumer can accept was
 * redelivered on every claim for the life of the database.
 */
const DEFAULT_OUTBOX_MAX_ATTEMPTS = 10;

// ---------------------------------------------------------------------------
// JSON / bool helpers
// ---------------------------------------------------------------------------

function toJson(value: unknown): string {
  return JSON.stringify(value ?? null);
}
/** For NOT NULL columns that always hold valid JSON (written with a default). */
function parseJson<T>(text: string): T {
  return JSON.parse(text) as T;
}
/** For nullable columns where NULL means "the field was absent". */
function parseNullableJson<T>(text: string | null): T | undefined {
  return text === null ? undefined : (JSON.parse(text) as T);
}

/**
 * A caller-supplied instant, rendered as the UTC ISO string this store's
 * TEXT comparisons need.
 *
 * `available_at <= $now` and `ORDER BY available_at` are STRING comparisons.
 * ISO-8601 is only lexicographically ordered within one representation, so an
 * offset-form value (`2026-01-01T01:30:00-05:00`, i.e. 06:30Z) sorts before a
 * `Z` value naming an earlier instant and gets claimed hours before it is due —
 * while the memory adapter, which parses to a `Date`, gets it right. That is
 * the worst kind of adapter divergence: silent, and only in the retry paths.
 *
 * An unparsable value is REFUSED rather than stored. Storing it would poison
 * every later comparison against that column (SQLite happily orders
 * `"tomorrow"` against a timestamp), and the caller can still fix its input.
 */
function normalizeInstant(value: string, field: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    throw new AgentKitHostError(
      "invalid_timestamp",
      `${field} is not a parsable instant: ${JSON.stringify(value)}.`,
      { field, value },
    );
  }
  return parsed.toISOString();
}

// ---------------------------------------------------------------------------
// Message content: one TEXT column plus a format tag
// ---------------------------------------------------------------------------

/** The two values `messages.content_format` may hold. */
type ContentFormat = "text" | "parts";

/** A message body, ready for the `content` + `content_format` columns. */
interface EncodedContent {
  content: string;
  format: ContentFormat;
}

/**
 * Split a message body into the two columns that store it.
 *
 * A string is written VERBATIM with format `'text'` — byte-identical to every
 * row this adapter wrote before v5, so nothing about the ordinary conversation
 * changed shape. A parts array is `JSON.stringify`d with format `'parts'`.
 *
 * `JSON.stringify` is canonical enough here because it is the ONLY writer: key
 * order comes from the part objects the contract defines, so
 * `decode(encode(x))` is deep-equal to `x` for every value the closed part union
 * admits. It is deliberately not claimed to be canonical across writers — this
 * column is never hashed, compared as text, or indexed on equality.
 */
function encodeContent(content: AiMessageContent): EncodedContent {
  return typeof content === "string"
    ? { content, format: "text" }
    : { content: JSON.stringify(content), format: "parts" };
}

/**
 * Rebuild a message body from its two columns.
 *
 * Anything that is not exactly `'parts'` reads as text, INCLUDING a value this
 * build does not recognize. That is the safe direction: a body handed back as
 * the string it is stored as is readable, where a body a future format tag made
 * unparseable would take the whole conversation down — and `assertSchemaVersion`
 * has already refused any database a different build wrote.
 */
function decodeContent(content: string, format: string): AiMessageContent {
  return format === "parts"
    ? (JSON.parse(content) as AiContentPart[])
    : content;
}

function toIntBool(value: boolean): number {
  return value ? 1 : 0;
}
function fromIntBool(value: number): boolean {
  return value === 1;
}
function toOptionalIntBool(value: boolean | undefined): number | null {
  return value === undefined ? null : value ? 1 : 0;
}
function fromOptionalIntBool(value: number | null): boolean | undefined {
  return value === null ? undefined : value === 1;
}
/**
 * Whether a driver error is a unique-constraint violation — ANY of them, on
 * whatever index tripped.
 *
 * `createTask` reads that as "duplicate task id", which is correct only because
 * `tasks` has exactly ONE unique constraint: its `task_id` primary key. Add a
 * second unique index to that table and this predicate stops being a
 * translation and starts being a lie — narrow it (to the constraint name, or by
 * probing for the existing row) at the call site before you do.
 */
function isConstraintError(err: unknown): boolean {
  const code = (err as { code?: string } | null)?.code;
  return (
    code === "SQLITE_CONSTRAINT_UNIQUE" ||
    code === "SQLITE_CONSTRAINT_PRIMARYKEY"
  );
}

/**
 * Whether a driver error means "someone else holds the lock, try again" —
 * SQLITE_BUSY and its variants, plus SQLITE_LOCKED.
 *
 * A prefix match rather than an equality one: SQLite reports extended codes
 * (`SQLITE_BUSY_SNAPSHOT`, `SQLITE_BUSY_TIMEOUT`) whose meaning for a caller is
 * identical — the write lock was not available — and enumerating them would
 * only mean missing whichever one a future SQLite adds.
 */
function isBusyError(err: unknown): boolean {
  const code = (err as { code?: string } | null)?.code;
  return (
    typeof code === "string" &&
    (code.startsWith("SQLITE_BUSY") || code.startsWith("SQLITE_LOCKED"))
  );
}

// ---------------------------------------------------------------------------
// Row shapes (snake_case columns as bun:sqlite returns them)
// ---------------------------------------------------------------------------

interface ChatRow {
  id: string;
  title: string | null;
  created_at: string;
  updated_at: string;
  metadata: string;
  archived: number;
}
interface MessageRow {
  id: string;
  chat_id: string;
  run_id: string | null;
  role: string;
  content: string;
  content_format: string;
  order_key: number;
  tool_call_id: string | null;
  tool_calls: string | null;
  model_result_json: string | null;
  parent_message_id: string | null;
  depth: number;
  branch_index: number;
  active: number;
  metadata: string;
  created_at: string;
}
interface TaskRow {
  task_id: string;
  kind: string;
  scope_id: string;
  status: string;
  priority: number;
  enqueued_at: string;
  available_at: string;
  started_at: string | null;
  finished_at: string | null;
  payload: string;
  parent_task_id: string | null;
  depends_on: string | null;
  progress: string | null;
  error: string | null;
  attempt_count: number;
  poison_count: number;
  dead_lettered_at: string | null;
  dead_letter_reason: string | null;
}
interface AttemptRow {
  attempt_id: string;
  task_id: string;
  attempt_number: number;
  status: string;
  owner_id: string;
  started_at: string;
  ended_at: string | null;
  error: string | null;
}
interface LeaseRow {
  task_id: string;
  attempt_id: string;
  owner_id: string;
  lease_token: string;
  fencing_token: number;
  expires_at: string;
}
interface TaskEventRow {
  task_id: string;
  seq: number;
  event_id: string;
  attempt_id: string | null;
  type: string;
  timestamp: string;
  payload: string;
}
interface OutboxRow {
  id: string;
  topic: string;
  run_id: string | null;
  payload: string;
  created_at: string;
  available_at: string;
  attempts: number;
  published_at: string | null;
  last_error: string | null;
}
interface ProposalRow {
  id: string;
  chat_id: string;
  run_id: string | null;
  scope_key: string;
  action_id: string | null;
  tool_name: string;
  kind: string;
  risk: string;
  status: string;
  envelope: string;
  operations: string;
  warnings: string;
  truncated: number;
  revision_at_create: string | null;
  operation_id: string | null;
  decision: string | null;
  reason: string | null;
  created_at: string;
  decided_at: string | null;
  applied_at: string | null;
}
interface ProposalOutcomeRow {
  operation_id: string;
  status: string;
  applied_ops: number;
  failed_ops: string;
  result_json: string | null;
  revision: string | null;
}
interface ProviderRow {
  id: string;
  label: string;
  kind: string;
  base_url: string;
  api_key: string | null;
  default_model: string;
  enabled: number;
  extra_headers: string | null;
  metadata: string | null;
}
interface ProviderModelRow {
  provider_id: string;
  model_id: string;
  display_name: string | null;
  context_window_tokens: number | null;
  supports_tool_calling: number | null;
  fetched_at: string;
}
interface ProviderCapabilitiesRow {
  provider_id: string;
  streaming: number;
  tool_calling: number;
  model_list: number;
  vision: number | null;
  json_mode: number | null;
  max_context_tokens: number | null;
  checked_at: string | null;
  warning: string | null;
}
interface SettingsRow {
  id: number;
  default_provider_id: string | null;
  default_model: string | null;
  context_size_preference: string;
  write_policy_mode: string;
  allow_raw_tool_data: number;
  max_tool_iterations: number | null;
  /** Named `_mode` because `provider_capabilities.tool_calling` is a boolean. */
  tool_calling_mode: string;
  metadata: string;
}

// ---------------------------------------------------------------------------
// Row -> record mapping
// ---------------------------------------------------------------------------

function chatFromRow(row: ChatRow): ChatRecord {
  return {
    id: row.id,
    ...(row.title === null ? {} : { title: row.title }),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
    metadata: parseJson<Record<string, unknown>>(row.metadata),
    archived: fromIntBool(row.archived),
  };
}

function messageFromRow(row: MessageRow): MessageRecord {
  return {
    id: row.id,
    chatId: row.chat_id,
    ...(row.run_id === null ? {} : { runId: row.run_id }),
    role: row.role as MessageRecord["role"],
    content: decodeContent(row.content, row.content_format),
    orderKey: row.order_key,
    ...(row.tool_call_id === null ? {} : { toolCallId: row.tool_call_id }),
    ...(row.tool_calls === null
      ? {}
      : { toolCalls: parseJson<AiToolCall[]>(row.tool_calls) }),
    ...(row.model_result_json === null
      ? {}
      : { modelResultJson: row.model_result_json }),
    ...(row.parent_message_id === null
      ? {}
      : { parentMessageId: row.parent_message_id }),
    depth: row.depth,
    branchIndex: row.branch_index,
    active: fromIntBool(row.active),
    metadata: parseJson<Record<string, unknown>>(row.metadata),
    createdAt: row.created_at,
  };
}

function taskFromRow(row: TaskRow): TaskRecord {
  return {
    taskId: row.task_id,
    kind: row.kind,
    scopeId: row.scope_id,
    status: row.status as TaskStatus,
    priority: row.priority,
    enqueuedAt: row.enqueued_at,
    availableAt: row.available_at,
    ...(row.started_at === null ? {} : { startedAt: row.started_at }),
    ...(row.finished_at === null ? {} : { finishedAt: row.finished_at }),
    payload: parseJson<Record<string, unknown>>(row.payload),
    ...(row.parent_task_id === null
      ? {}
      : { parentTaskId: row.parent_task_id }),
    ...(row.depends_on === null
      ? {}
      : { dependsOn: parseJson<string[]>(row.depends_on) }),
    ...(row.progress === null
      ? {}
      : { progress: parseJson<Record<string, unknown>>(row.progress) }),
    ...(row.error === null ? {} : { error: row.error }),
    attemptCount: row.attempt_count,
    poisonCount: row.poison_count,
    ...(row.dead_lettered_at === null
      ? {}
      : { deadLetteredAt: row.dead_lettered_at }),
    ...(row.dead_letter_reason === null
      ? {}
      : { deadLetterReason: row.dead_letter_reason }),
  };
}

function attemptFromRow(row: AttemptRow): AttemptRecord {
  return {
    attemptId: row.attempt_id,
    taskId: row.task_id,
    attemptNumber: row.attempt_number,
    status: row.status as AttemptStatus,
    ownerId: row.owner_id,
    startedAt: row.started_at,
    ...(row.ended_at === null ? {} : { endedAt: row.ended_at }),
    ...(row.error === null ? {} : { error: row.error }),
  };
}

function leaseFromRow(row: LeaseRow): Lease {
  return {
    taskId: row.task_id,
    attemptId: row.attempt_id,
    ownerId: row.owner_id,
    leaseToken: row.lease_token,
    fencingToken: row.fencing_token,
    expiresAt: row.expires_at,
  };
}

function proposalFromRow(row: ProposalRow): ProposalRecord {
  const decision = parseNullableJson<ProposalDecision>(row.decision);
  return {
    id: row.id,
    chatId: row.chat_id,
    ...(row.run_id === null ? {} : { runId: row.run_id }),
    scopeKey: row.scope_key,
    ...(row.action_id === null ? {} : { actionId: row.action_id }),
    toolName: row.tool_name,
    kind: row.kind,
    risk: row.risk as RiskLevel,
    status: row.status as ProposalStatus,
    envelope: parseJson<Record<string, unknown>>(row.envelope),
    operations: parseJson<unknown[]>(row.operations),
    warnings: parseJson<string[]>(row.warnings),
    truncated: fromIntBool(row.truncated),
    ...(row.revision_at_create === null
      ? {}
      : { revisionAtCreate: row.revision_at_create }),
    ...(row.operation_id === null ? {} : { operationId: row.operation_id }),
    ...(decision === undefined ? {} : { decision }),
    ...(row.reason === null ? {} : { reason: row.reason }),
    createdAt: row.created_at,
    ...(row.decided_at === null ? {} : { decidedAt: row.decided_at }),
    ...(row.applied_at === null ? {} : { appliedAt: row.applied_at }),
  };
}

function outcomeFromRow(row: ProposalOutcomeRow): ApplyOutcome {
  return {
    status: row.status as ApplyOutcome["status"],
    appliedOps: row.applied_ops,
    failedOps: parseJson<{ opIndex: number; error: string }[]>(row.failed_ops),
    ...(row.result_json === null ? {} : { resultJson: row.result_json }),
    ...(row.revision === null ? {} : { revision: row.revision }),
  };
}

function providerFromRow(row: ProviderRow): AiProviderConfig {
  return {
    id: row.id,
    label: row.label,
    kind: row.kind,
    baseUrl: row.base_url,
    ...(row.api_key === null ? {} : { apiKey: row.api_key }),
    defaultModel: row.default_model,
    enabled: fromIntBool(row.enabled),
    ...(row.extra_headers === null
      ? {}
      : { extraHeaders: parseJson<Record<string, string>>(row.extra_headers) }),
    ...(row.metadata === null
      ? {}
      : { metadata: parseJson<Record<string, unknown>>(row.metadata) }),
  };
}

function modelFromRow(row: ProviderModelRow): AiProviderModel {
  return {
    providerId: row.provider_id,
    modelId: row.model_id,
    displayName: row.display_name,
    ...(row.context_window_tokens === null
      ? {}
      : { contextWindowTokens: row.context_window_tokens }),
    ...(row.supports_tool_calling === null
      ? {}
      : {
          supportsToolCalling: fromOptionalIntBool(row.supports_tool_calling),
        }),
    fetchedAt: row.fetched_at,
  };
}

function capabilitiesFromRow(
  row: ProviderCapabilitiesRow,
): AiProviderCapabilities {
  return {
    streaming: fromIntBool(row.streaming),
    toolCalling: fromIntBool(row.tool_calling),
    modelList: fromIntBool(row.model_list),
    ...(row.vision === null ? {} : { vision: fromIntBool(row.vision) }),
    ...(row.json_mode === null ? {} : { jsonMode: fromIntBool(row.json_mode) }),
    ...(row.max_context_tokens === null
      ? {}
      : { maxContextTokens: row.max_context_tokens }),
    ...(row.checked_at === null ? {} : { checkedAt: row.checked_at }),
    ...(row.warning === null ? {} : { warning: row.warning }),
  };
}

function settingsFromRow(row: SettingsRow): AssistantSettings {
  return {
    ...(row.default_provider_id === null
      ? {}
      : { defaultProviderId: row.default_provider_id }),
    ...(row.default_model === null ? {} : { defaultModel: row.default_model }),
    contextSizePreference:
      row.context_size_preference as AssistantSettings["contextSizePreference"],
    writePolicyMode: row.write_policy_mode as WritePolicyMode,
    allowRawToolData: fromIntBool(row.allow_raw_tool_data),
    ...(row.max_tool_iterations === null
      ? {}
      : { maxToolIterations: row.max_tool_iterations }),
    toolCalling: row.tool_calling_mode as ToolCallingMode,
    metadata: parseJson<Record<string, unknown>>(row.metadata),
  };
}

function outboxFromRow(row: OutboxRow): OutboxRecord {
  return {
    id: row.id,
    topic: row.topic,
    ...(row.run_id === null ? {} : { runId: row.run_id }),
    payload: parseJson<Record<string, unknown>>(row.payload),
    createdAt: row.created_at,
    availableAt: row.available_at,
    attempts: row.attempts,
    ...(row.published_at === null ? {} : { publishedAt: row.published_at }),
    ...(row.last_error === null ? {} : { lastError: row.last_error }),
  };
}

// ---------------------------------------------------------------------------
// Connection wrapper: flattened BEGIN IMMEDIATE / COMMIT / ROLLBACK
// ---------------------------------------------------------------------------

/** Bound-parameter bag: `$name` keys, scalar/null values — the named-params style used throughout this file. */
type Params = Record<string, string | number | boolean | bigint | null>;

/**
 * Identity of one open async transaction — a token, not a counter.
 *
 * "A transaction is open" and "MY transaction is open" are different questions,
 * and only the second one may flatten; see
 * {@link SqliteConnection.withAsyncTx}. Deliberately opaque: nothing reads a
 * field on it, callers only ever compare it by identity.
 */
interface TxOwner {
  readonly open: true;
}

/**
 * Shared by every sub-store so a multi-statement operation (inside one port
 * method, or spanning several via {@link SqliteAssistantStore.transaction})
 * commits or rolls back as one unit. Also the single seam where bun-types'
 * generic (array-rest) binding signature is cast to the named-params object
 * form its own runtime and JSDoc document (`db.run(sql, { $name: "foo" })`)
 * — the shipped `.d.ts` models positional array bindings precisely but not
 * that form, so callers here pass a plain `{ $x: ... }` object and this class
 * is the only place that casts it.
 *
 * `bun:sqlite` is synchronous and does not support nested transactions on one
 * connection (no savepoints in this v1 — see the class doc on
 * {@link SqliteAssistantStore.transaction}), so re-entrant calls FLATTEN into
 * the transaction already open: only the outermost `withTx`/`withAsyncTx`
 * issues BEGIN/COMMIT/ROLLBACK.
 *
 * WHICH CALLS COUNT AS RE-ENTRANT IS DECIDED BY OWNERSHIP, NOT BY DEPTH. A
 * raised `txDepth` says "a transaction is open", never "mine is open", and an
 * unrelated caller that flattened on it made its whole unit of work hostage to
 * a stranger's rollback: a second `AssistantStore.transaction` caller reported
 * a commit its neighbour's throw then erased, and a `claimNext` that landed in
 * a host transaction had its claim reverted under a worker already holding the
 * lease. So the two helpers answer the question differently:
 *
 * - {@link withTx} (synchronous) still flattens on depth. It holds the thread
 *   from BEGIN to COMMIT, so nothing can interleave WITH it, and flattening is
 *   what lets another object over this same handle — `SqliteMcpServerConfigStore`
 *   — write inside an open transaction instead of deadlocking against it.
 * - {@link withAsyncTx} flattens only for the caller holding the CURRENT owner
 *   token. Every other caller queues behind {@link txGate} and gets its own
 *   BEGIN, so one caller's rollback can only ever discard that caller's work.
 *
 * That left one hole, which {@link whenFree} closes: an unrelated caller's
 * SYNCHRONOUS port write, issued while an async transaction sat on an `await`,
 * still joined that transaction on plain `withTx` — and was erased by a
 * rollback it had nothing to do with. Every WRITE method of every sub-store now
 * goes through `whenFree`, which waits out a transaction it does not own before
 * opening its own. READS still join: they take no locks worth serializing, and
 * a read that queued behind a transaction it is not part of would turn every
 * `getTask` inside a busy host into a wait.
 *
 * ── SEVERAL HANDLES OVER ONE FILE ─────────────────────────────────────────
 *
 * Supported, and this class is where the support lives. Two
 * {@link SqliteAssistantStore} instances on one path — two worker processes, or
 * two connections in one process — are two connections contending for SQLite's
 * single write lock, and this connection's own {@link txGate} means nothing
 * across that boundary. `BEGIN IMMEDIATE` is what keeps them correct;
 * what keeps them USABLE is waiting for the lock instead of failing on it, and
 * the two waits are deliberately different:
 *
 * - SYNCHRONOUS transactions ({@link withTx}) wait inside SQLite, via the
 *   `PRAGMA busy_timeout` the store sets on open. They cannot await, and when
 *   the lock holder is another OS process, parking this thread is exactly the
 *   right thing to do.
 * - ASYNCHRONOUS transactions ({@link withAsyncTx} — `claimNext` and
 *   `AssistantStore.transaction`) wait on the EVENT LOOP instead, and set
 *   `busy_timeout` to zero while they try. They hold the lock across `await`s,
 *   so the holder may well be this same process's other handle — and then the
 *   thread SQLite would park is the only thread that could ever release the
 *   lock. Sleeping on it turns a moment of contention into a deadlock that
 *   lasts the whole timeout and then fails anyway.
 */
class SqliteConnection {
  private txDepth = 0;

  /**
   * The FIFO every top-level {@link withAsyncTx} queues on: each call chains
   * onto the previous one's SETTLED signal, so async transactions run one at a
   * time on this connection, in call order.
   */
  private txGate: Promise<void> = Promise.resolve();

  /** Token of the async transaction currently open, `null` when there is none. */
  private currentOwner: TxOwner | null = null;

  constructor(
    readonly db: Database,
    /** Ceiling on how long either wait above will keep trying. */
    private readonly busyTimeoutMs: number,
  ) {}

  run(sql: string, params?: Params): Changes {
    // bun-types' generic for Database.run (`...bindings: ParamsType[]` where
    // `ParamsType extends SQLQueryBindings[]`) models an array of bindings
    // ARRAYS, which does not match its own documented single-object calling
    // convention (`db.run(sql, { $name: "foo" })`, per the class's own
    // JSDoc). Re-typing `this.db` sidesteps that mismatched generic while
    // still calling `run` AS A METHOD on the same instance (not a detached
    // function reference — bun:sqlite's native binding needs `this` bound to
    // the Database instance, so extracting `db.run` into a bare variable and
    // calling it unbound breaks at runtime even though it type-checks).
    const db = this.db as unknown as {
      run(sql: string, params?: Params): Changes;
    };
    return params === undefined ? db.run(sql) : db.run(sql, params);
  }

  // biome-ignore lint/suspicious/noExplicitAny: driver boundary — bun:sqlite rows are untyped; every call site casts to its own Row type immediately
  get(sql: string, params?: Params): any {
    const stmt = this.db.query(sql);
    return params === undefined ? stmt.get() : stmt.get(params);
  }

  // biome-ignore lint/suspicious/noExplicitAny: driver boundary — see get()
  all(sql: string, params?: Params): any[] {
    const stmt = this.db.query(sql);
    return params === undefined ? stmt.all() : stmt.all(params);
  }

  exec(sql: string): void {
    this.db.exec(sql);
  }

  /**
   * Synchronous transaction helper for a single port method's own SQL.
   *
   * THE BEGIN RUNS BEFORE `txDepth` MOVES, and that order is load-bearing: a
   * `BEGIN IMMEDIATE` that throws (another connection holds the write lock)
   * used to leave the counter raised forever, so every later call on this
   * connection took the "already in a transaction" branch and ran with no
   * BEGIN, no COMMIT and no ROLLBACK — atomicity silently gone for the life of
   * the connection. Raising the counter only once the transaction really is
   * open makes the pair exception-safe without changing the flatten-on-reentry
   * semantics: a raised counter still means, exactly, "a transaction is open".
   */
  withTx<T>(fn: () => T): T {
    if (this.txDepth > 0) return fn();
    // Waits inside SQLite for up to `busy_timeout` — see the class doc.
    this.exec("BEGIN IMMEDIATE");
    this.txDepth += 1;
    try {
      const result = fn();
      this.exec("COMMIT");
      return result;
    } catch (err) {
      this.rollback();
      throw err;
    } finally {
      this.txDepth -= 1;
    }
  }

  /**
   * Async transaction helper for {@link AssistantStore.transaction} and
   * `claimNext`: `fn` may `await` between its statements, so this transaction
   * is held across turns of the event loop, where anybody else's callback can
   * run.
   *
   * ONE AT A TIME PER CONNECTION, IN CALL ORDER. A caller that arrives while a
   * transaction is open waits on {@link txGate} for it to settle instead of
   * joining it — joining is what let one caller's rollback discard another
   * caller's finished work (see the class doc). The one exception is the caller
   * that IS the open transaction: `owner` names it, and a call carrying the
   * token of the transaction currently running flattens into it, since there
   * are no savepoints to nest with. That is how a nested `transaction()` and a
   * `claimNext` issued through the `tx` view stay inside the unit their caller
   * opened, while the same calls made by anyone else queue.
   *
   * The gate wait is deliberately NOT bounded by `busyTimeoutMs`. That budget
   * exists for the write lock, which another process owns and may never
   * release; the gate is this process's own queue, and its holder always
   * settles.
   */
  async withAsyncTx<T>(
    fn: (owner: TxOwner) => Promise<T>,
    owner?: TxOwner,
  ): Promise<T> {
    // Decided SYNCHRONOUSLY, on the caller's own turn: `currentOwner` is read
    // before the first await, so it still describes the transaction this call
    // was issued from.
    if (owner !== undefined && owner === this.currentOwner) return fn(owner);
    const run = this.txGate.then(() => this.beginExclusive(fn));
    // The gate carries the SETTLED signal only: the next caller waits for this
    // one to finish, and must not inherit its rejection.
    this.txGate = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  /**
   * Run `fn` in a synchronous transaction of its own, once this connection has
   * no async transaction open that `owner` is not part of.
   *
   * THE GATE CHECK AND THE `withTx` ARE ONE TICK, and that is the whole point.
   * The obvious shape — an `await ready()` helper followed by the caller's own
   * `withTx` — leaves a microtask gap: a transaction already queued on
   * {@link txGate} runs its BEGIN in that gap, and the write then flattens into
   * the stranger's transaction after all. Here the `while` exits and
   * `this.withTx(fn)` runs in the same synchronous continuation, so nothing can
   * open a transaction in between.
   *
   * The caller that IS the open transaction passes its `owner` and never waits:
   * the loop's condition is false for it, so `fn` runs immediately and
   * {@link withTx} flattens it into the transaction it belongs to. That is what
   * keeps `tx.conversations.updateChat(...)` inside its caller's unit — and
   * what keeps `claimNext`'s own nested writes from waiting on the transaction
   * they are running inside.
   *
   * A single-statement write is wrapped too. The BEGIN/COMMIT costs a pair of
   * pragma-free statements and buys the one thing the bare `run` did not have:
   * a blast radius of exactly this write.
   */
  async whenFree<T>(fn: () => T, owner?: TxOwner): Promise<T> {
    while (this.currentOwner !== null && owner !== this.currentOwner) {
      // Re-read each turn: `txGate` is the TAIL of the queue, so waiting on it
      // also lets everything already queued go first — a write cannot jump the
      // line, and cannot be starved by callers that arrive after it either
      // (they chain onto the same promise this one is already waiting on).
      await this.txGate;
    }
    return this.withTx(fn);
  }

  /**
   * One async transaction, with {@link txGate} already held by this call.
   *
   * Same BEGIN-then-increment ordering as {@link withTx}, for the same reason,
   * and the same exception-safety: a lock this call never won leaves the
   * counter untouched, and the owner token is minted only once the transaction
   * really is open.
   */
  private async beginExclusive<T>(
    fn: (owner: TxOwner) => Promise<T>,
  ): Promise<T> {
    const deadline = Date.now() + this.busyTimeoutMs;
    // Whoever holds the lock here is on ANOTHER HANDLE: the gate keeps this
    // handle's async transactions apart, and a synchronous one cannot still be
    // open across the await below. So this is the cross-connection wait the
    // class doc describes — yield and retry rather than park the thread, see
    // tryBeginImmediate.
    for (let attempt = 0; ; attempt += 1) {
      const busy = this.tryBeginImmediate();
      if (busy === null) break;
      if (Date.now() >= deadline) throw busy;
      // A macrotask, not a microtask: the lock holder's next step may be queued
      // behind one, and a microtask-only yield would spin without ever letting
      // it run. The short backoff keeps a long wait from burning the loop.
      await new Promise((resolve) =>
        setTimeout(resolve, Math.min(attempt, 10)),
      );
    }
    const owner: TxOwner = { open: true };
    this.currentOwner = owner;
    try {
      const result = await fn(owner);
      this.exec("COMMIT");
      return result;
    } catch (err) {
      this.rollback();
      throw err;
    } finally {
      // Cleared, not restored: the gate guarantees there was no async
      // transaction underneath this one.
      this.currentOwner = null;
      this.txDepth -= 1;
    }
  }

  /**
   * One non-blocking attempt at the write lock: `null` when the transaction is
   * open and `txDepth` has been raised, the SQLITE_BUSY error when it is not.
   *
   * `busy_timeout` is dropped to zero for the attempt and restored after,
   * because SQLite's own wait PARKS THE CALLING THREAD — and when the holder is
   * this process's other handle, that thread is the only one that could ever
   * run the holder's continuation and commit. Measured against a real
   * two-handle claim, the parking version stalls for the whole timeout and then
   * raises SQLITE_BUSY anyway; yielding between attempts resolves the same
   * contention in single-digit milliseconds. Synchronous callers cannot do
   * this, which is why {@link withTx} keeps SQLite's wait — for the
   * cross-PROCESS holder it is aimed at, parking the thread is right.
   */
  private tryBeginImmediate(): unknown | null {
    try {
      this.exec("PRAGMA busy_timeout = 0");
      this.exec("BEGIN IMMEDIATE");
    } catch (err) {
      if (isBusyError(err)) return err;
      throw err;
    } finally {
      this.exec(`PRAGMA busy_timeout = ${this.busyTimeoutMs}`);
    }
    // Raised here, with no await since the BEGIN — see withAsyncTx's comment.
    this.txDepth += 1;
    return null;
  }

  private rollback(): void {
    try {
      this.exec("ROLLBACK");
    } catch {
      // No transaction to roll back — the connection died, or something below
      // COMMIT already ended it. Either way there is nothing left to undo.
    }
  }
}

// ---------------------------------------------------------------------------
// ConversationStore
// ---------------------------------------------------------------------------

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

class SqliteConversationStore implements ConversationStore {
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

// ---------------------------------------------------------------------------
// TaskStore
// ---------------------------------------------------------------------------

/**
 * The statuses that make a scope undeletable — the same two
 * `ConversationService.deleteChat` refuses on, restated here because the STORE
 * owns the guarantee (see `TaskStore.deleteByScope`) and a store cannot import
 * a service's private constant.
 *
 * Typed as `TaskStatus[]` on purpose: a status renamed out of the union fails
 * to compile here rather than turning this guard into a filter that matches
 * nothing.
 */
const BUSY_TASK_STATUSES: readonly TaskStatus[] = Object.freeze([
  "running",
  "waiting_approval",
]);

/**
 * {@link BUSY_TASK_STATUSES} as an SQL `IN` list, built from the same constant
 * so the two cannot drift. Interpolated rather than bound because these are
 * this module's own compile-time literals, never caller input.
 */
const BUSY_TASK_STATUS_SQL = BUSY_TASK_STATUSES.map(
  (status) => `'${status}'`,
).join(", ");

/**
 * Refuse a scope delete while anything in it is live, naming what is holding it.
 *
 * The message and `details` shape are deliberately byte-identical to the ones
 * `ConversationService.deleteChat` raises from its own fast-path check: a
 * caller (or a transport mapping `chat_busy` to a 409) must not be able to tell
 * which of the two layers refused.
 */
function assertScopeNotBusy(
  scopeId: string,
  busy: readonly { task_id: string; status: string }[],
): void {
  if (busy.length === 0) return;
  throw new ChatBusyError(
    `Chat ${scopeId} has ${busy.length} task(s) still running or awaiting approval; cancel or await them before deleting.`,
    {
      chatId: scopeId,
      taskIds: busy.map((row) => row.task_id),
      statuses: busy.map((row) => row.status),
    },
  );
}

class SqliteTaskStore implements TaskStore {
  private readonly aging: ResolvedTaskAging;

  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
    private readonly leaseTtlMs: number = DEFAULT_LEASE_TTL_MS,
    aging: TaskAgingOptions = {},
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {
    this.aging = resolveTaskAging(aging);
  }

  async createTask(input: CreateTaskInput): Promise<TaskRecord> {
    const now = this.clock.nowIso();
    // Normalized before it is stored — `selectClaimCandidates` compares this
    // column as TEXT; see `normalizeInstant`.
    const availableAt =
      input.availableAt === undefined
        ? now
        : normalizeInstant(input.availableAt, "availableAt");
    const priority = input.priority ?? 0;
    // Immutable after create, so the array is copied out of the caller's hands
    // before it is serialized — and normalized to NULL when absent, which is
    // what `taskFromRow` reads back as "no gate".
    const dependsOn =
      input.dependsOn === undefined ? null : [...input.dependsOn];
    try {
      // Inside a transaction with the INSERT: the existence proof and the write
      // that relies on it must not be separated by another connection's commit,
      // or a concurrent delete between them would leave the dangling edge this
      // check exists to prevent.
      await this.conn.whenFree(() => {
        this.assertDependenciesExist(
          input.taskId,
          input.parentTaskId,
          dependsOn,
        );
        this.conn.run(
          `INSERT INTO tasks
             (task_id, kind, scope_id, status, priority, enqueued_at, available_at, payload,
              parent_task_id, depends_on, attempt_count, poison_count)
           VALUES
             ($taskId, $kind, $scopeId, 'queued', $priority, $now, $availableAt, $payload,
              $parentTaskId, $dependsOn, 0, 0)`,
          {
            $taskId: input.taskId,
            $kind: input.kind,
            $scopeId: input.scopeId,
            $priority: priority,
            $now: now,
            $availableAt: availableAt,
            $payload: toJson(input.payload),
            $parentTaskId: input.parentTaskId ?? null,
            $dependsOn: dependsOn === null ? null : toJson(dependsOn),
          },
        );
      }, this.txOwner);
    } catch (err) {
      // The PK collision IS the idempotency guard doing its job; leaking the
      // raw SQLite constraint error would make every caller match on a driver
      // string to tell "already submitted" from "the database is broken".
      // `isConstraintError` does not check WHICH constraint tripped, which is
      // sound only while `task_id`'s primary key is the sole unique constraint
      // on `tasks` — see its doc comment before adding another unique index.
      if (isConstraintError(err)) {
        throw new DuplicateTaskError(`Task already exists: ${input.taskId}.`, {
          taskId: input.taskId,
          cause: String(err),
        });
      }
      throw err;
    }
    return {
      taskId: input.taskId,
      kind: input.kind,
      scopeId: input.scopeId,
      status: "queued",
      priority,
      enqueuedAt: now,
      availableAt,
      payload: input.payload,
      ...(input.parentTaskId === undefined
        ? {}
        : { parentTaskId: input.parentTaskId }),
      ...(dependsOn === null ? {} : { dependsOn }),
      attemptCount: 0,
      poisonCount: 0,
    };
  }

  async getTask(taskId: string): Promise<TaskRecord | null> {
    const row = this.selectTaskRow(taskId);
    return row ? taskFromRow(row) : null;
  }

  async listChildren(taskId: string): Promise<TaskRecord[]> {
    const rows = this.conn.all(
      `SELECT * FROM tasks WHERE parent_task_id = $parentTaskId ORDER BY enqueued_at ASC, rowid ASC`,
      { $parentTaskId: taskId },
    ) as TaskRow[];
    return rows.map(taskFromRow);
  }

  async listByScope(scopeId: string): Promise<TaskRecord[]> {
    const rows = this.conn.all(
      `SELECT * FROM tasks WHERE scope_id = $scopeId ORDER BY enqueued_at ASC, rowid ASC`,
      { $scopeId: scopeId },
    ) as TaskRow[];
    return rows.map(taskFromRow);
  }

  /**
   * Delete a scope's tasks with everything hanging off them, in ONE
   * transaction — unless something in the scope is still live, in which case
   * NOTHING is deleted and {@link ChatBusyError} is raised.
   *
   * THE BUSY CHECK IS THE FIRST STATEMENT OF THAT SAME TRANSACTION, and there
   * is no `await` between it and the deletes. That is the whole point of the
   * guard living here rather than only in `ConversationService.deleteChat`: the
   * service's check runs inside an async transaction, and a concurrent
   * `claimNext` on this connection FLATTENS into it (see
   * {@link SqliteConnection}) — so a task can go `queued → running` between the
   * service's check and this call. Nothing can run between statements of a
   * synchronous `withTx` body, so a check made here holds for the deletes that
   * follow it. See `TaskStore.deleteByScope`.
   *
   * Children before parents, because `task_attempts` and `leases` carry real
   * foreign keys to `tasks` and SQLite is not going to let a task row leave
   * while either still names it. `task_events` has no FK — it is keyed by
   * `task_id` alone, deliberately, so an event log outlives the attempt that
   * wrote it — which is exactly why it has to be deleted explicitly here rather
   * than swept up by a cascade that does not exist.
   */
  async deleteByScope(scopeId: string): Promise<number> {
    return this.conn.whenFree(() => {
      const scoped = `SELECT task_id FROM tasks WHERE scope_id = $scopeId`;
      const params: Params = { $scopeId: scopeId };
      // Same ordering as `listByScope`, so the ids and statuses this refusal
      // names are the ones the caller's own pre-check would have listed.
      assertScopeNotBusy(
        scopeId,
        this.conn.all(
          `SELECT task_id, status FROM tasks
            WHERE scope_id = $scopeId AND status IN (${BUSY_TASK_STATUS_SQL})
            ORDER BY enqueued_at ASC, rowid ASC`,
          params,
        ) as { task_id: string; status: string }[],
      );
      this.conn.run(
        `DELETE FROM task_events WHERE task_id IN (${scoped})`,
        params,
      );
      this.conn.run(`DELETE FROM leases WHERE task_id IN (${scoped})`, params);
      this.conn.run(
        `DELETE FROM task_attempts WHERE task_id IN (${scoped})`,
        params,
      );
      return this.conn.run(
        `DELETE FROM tasks WHERE scope_id = $scopeId`,
        params,
      ).changes;
    }, this.txOwner);
  }

  async transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    return this.transitionTaskAs(this.txOwner, taskId, from, to, patch, opts);
  }

  /**
   * {@link transitionTask}, told which transaction it belongs to.
   *
   * `claimNext` settles and claims candidates through this rather than through
   * the public method: it is already inside its own async transaction, and the
   * public method would gate on `this.txOwner` — undefined on the root store —
   * and wait for the very transaction it is running in.
   */
  private async transitionTaskAs(
    owner: TxOwner | undefined,
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    const availableAt =
      patch?.availableAt === undefined
        ? null
        : normalizeInstant(patch.availableAt, "availableAt");
    return this.conn.whenFree(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      if (opts?.leaseToken !== undefined) {
        this.assertLeaseCurrent(taskId, opts.leaseToken);
      }
      const current = row.status as TaskStatus;
      if (!from.includes(current)) {
        throw new InvalidTaskTransitionError(
          `Task ${taskId} is ${current}, expected one of [${from.join(", ")}].`,
          { taskId, current, from, to },
        );
      }
      assertTaskTransition(current, to);
      const result = this.conn.run(
        `UPDATE tasks SET
           status = $status,
           started_at = COALESCE($startedAt, started_at),
           finished_at = COALESCE($finishedAt, finished_at),
           error = COALESCE($error, error),
           available_at = COALESCE($availableAt, available_at),
           priority = COALESCE($priority, priority),
           poison_count = COALESCE($poisonCount, poison_count),
           payload = COALESCE($payload, payload)
         WHERE task_id = $id AND status = $current`,
        {
          $status: to,
          $startedAt: patch?.startedAt ?? null,
          $finishedAt: patch?.finishedAt ?? null,
          $error: patch?.error ?? null,
          $availableAt: availableAt,
          $priority: patch?.priority ?? null,
          $poisonCount: patch?.poisonCount ?? null,
          $payload: patch?.payload !== undefined ? toJson(patch.payload) : null,
          $id: taskId,
          $current: current,
        },
      );
      if (result.changes === 0) {
        // Lost a race between the SELECT above and this UPDATE.
        throw new InvalidTaskTransitionError(
          `Task ${taskId} changed concurrently; expected one of [${from.join(", ")}].`,
          { taskId, from, to },
        );
      }
      return taskFromRow(this.selectTaskRow(taskId)!);
    }, owner);
  }

  async createAttempt(input: CreateAttemptInput): Promise<AttemptRecord> {
    return this.createAttemptAs(this.txOwner, input);
  }

  /** {@link createAttempt}, told which transaction it belongs to. */
  private async createAttemptAs(
    owner: TxOwner | undefined,
    input: CreateAttemptInput,
  ): Promise<AttemptRecord> {
    return this.conn.whenFree(() => {
      const task = this.selectTaskRow(input.taskId);
      if (!task)
        throw new RecordNotFoundError(`Task not found: ${input.taskId}`);
      const attemptNumber = task.attempt_count + 1;
      const startedAt = this.clock.nowIso();
      this.conn.run(`UPDATE tasks SET attempt_count = $n WHERE task_id = $id`, {
        $n: attemptNumber,
        $id: input.taskId,
      });
      this.conn.run(
        `INSERT INTO task_attempts (attempt_id, task_id, attempt_number, status, owner_id, started_at)
         VALUES ($attemptId, $taskId, $attemptNumber, 'running', $ownerId, $startedAt)`,
        {
          $attemptId: input.attemptId,
          $taskId: input.taskId,
          $attemptNumber: attemptNumber,
          $ownerId: input.ownerId,
          $startedAt: startedAt,
        },
      );
      return {
        attemptId: input.attemptId,
        taskId: input.taskId,
        attemptNumber,
        status: "running",
        ownerId: input.ownerId,
        startedAt,
      };
    }, owner);
  }

  async endAttempt(input: EndAttemptInput): Promise<AttemptRecord> {
    return this.conn.whenFree(() => {
      const row = this.conn.get(
        `SELECT * FROM task_attempts WHERE attempt_id = $id`,
        { $id: input.attemptId },
      ) as AttemptRow | null;
      if (!row) {
        throw new RecordNotFoundError(`Attempt not found: ${input.attemptId}`);
      }
      // The attempt names its task, so the ownership proof is read from the
      // same row the write is about — inside this transaction, next to it.
      if (input.leaseToken !== undefined) {
        this.assertLeaseCurrent(row.task_id, input.leaseToken);
      }
      const endedAt = this.clock.nowIso();
      this.conn.run(
        `UPDATE task_attempts SET status = $status, ended_at = $endedAt, error = COALESCE($error, error) WHERE attempt_id = $id`,
        {
          $status: input.status,
          $endedAt: endedAt,
          $error: input.error ?? null,
          $id: input.attemptId,
        },
      );
      return attemptFromRow({
        ...row,
        status: input.status,
        ended_at: endedAt,
        error: input.error ?? row.error,
      });
    }, this.txOwner);
  }

  async acquireLease(input: AcquireLeaseInput): Promise<Lease> {
    return this.acquireLeaseAs(this.txOwner, input);
  }

  /** {@link acquireLease}, told which transaction it belongs to. */
  private async acquireLeaseAs(
    owner: TxOwner | undefined,
    input: AcquireLeaseInput,
  ): Promise<Lease> {
    return this.conn.whenFree(() => {
      // Store-global monotonic fencing token, single-row counter table.
      this.conn.run(
        `UPDATE fencing_counter SET value = value + 1 WHERE id = 1`,
      );
      const counter = this.conn.get(
        `SELECT value FROM fencing_counter WHERE id = 1`,
      ) as { value: number };
      const fencingToken = counter.value;
      const leaseToken = `lease_${crypto.randomUUID()}`;
      const expiresAt = new Date(
        this.clock.now().getTime() + input.ttlMs,
      ).toISOString();
      // PK on task_id: this always mints a fresh lease, replacing whatever was
      // there — see the module doc on lease semantics.
      this.conn.run(
        `INSERT INTO leases (task_id, attempt_id, owner_id, lease_token, fencing_token, expires_at)
         VALUES ($taskId, $attemptId, $ownerId, $leaseToken, $fencingToken, $expiresAt)
         ON CONFLICT(task_id) DO UPDATE SET
           attempt_id = excluded.attempt_id,
           owner_id = excluded.owner_id,
           lease_token = excluded.lease_token,
           fencing_token = excluded.fencing_token,
           expires_at = excluded.expires_at`,
        {
          $taskId: input.taskId,
          $attemptId: input.attemptId,
          $ownerId: input.ownerId,
          $leaseToken: leaseToken,
          $fencingToken: fencingToken,
          $expiresAt: expiresAt,
        },
      );
      return {
        taskId: input.taskId,
        attemptId: input.attemptId,
        ownerId: input.ownerId,
        leaseToken,
        fencingToken,
        expiresAt,
      };
    }, owner);
  }

  /**
   * Extend a lease that is still alive.
   *
   * AN EXPIRED LEASE IS NOT RENEWABLE, even while its row survives — the row
   * only outlives the expiry until someone runs `expireStaleLeases`, and the
   * whole point of an expiry is that another owner may act on it from that
   * instant. Renewing across it would resurrect ownership recovery is entitled
   * to consider gone, and it would break `renewLease`'s second job: the runner
   * uses it as the fencing probe ("may I still write?"), and a probe that says
   * yes on an expired lease is the wrong answer to that question.
   */
  async renewLease(leaseToken: string, ttlMs: number): Promise<Lease> {
    return this.conn.whenFree(() => {
      const now = this.clock.now();
      const row = this.conn.get(
        `SELECT * FROM leases WHERE lease_token = $token`,
        {
          $token: leaseToken,
        },
      ) as LeaseRow | null;
      if (!row) {
        throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
          leaseToken,
        });
      }
      // `<=` matches `expireStaleLeases`, so the two never disagree about a
      // lease that expires exactly on the instant being asked about.
      if (new Date(row.expires_at).getTime() <= now.getTime()) {
        throw new LeaseLostError(
          `Lease token ${leaseToken} expired at ${row.expires_at}.`,
          { leaseToken, expiresAt: row.expires_at },
        );
      }
      const expiresAt = new Date(now.getTime() + ttlMs).toISOString();
      this.conn.run(
        `UPDATE leases SET expires_at = $expiresAt WHERE lease_token = $token`,
        { $expiresAt: expiresAt, $token: leaseToken },
      );
      return leaseFromRow({ ...row, expires_at: expiresAt });
    }, this.txOwner);
  }

  async releaseLease(leaseToken: string): Promise<void> {
    await this.conn.whenFree(() => {
      const result = this.conn.run(
        `DELETE FROM leases WHERE lease_token = $token`,
        {
          $token: leaseToken,
        },
      );
      if (result.changes === 0) {
        throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
          leaseToken,
        });
      }
    }, this.txOwner);
  }

  async expireStaleLeases(now: Date): Promise<Lease[]> {
    return this.conn.whenFree(() => {
      const nowIso = now.toISOString();
      const rows = this.conn.all(
        `SELECT * FROM leases WHERE expires_at <= $now`,
        {
          $now: nowIso,
        },
      ) as LeaseRow[];
      if (rows.length > 0) {
        this.conn.run(`DELETE FROM leases WHERE expires_at <= $now`, {
          $now: nowIso,
        });
      }
      return rows.map(leaseFromRow);
    }, this.txOwner);
  }

  async appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void> {
    if (events.length === 0) return;
    await this.conn.whenFree(() => {
      const lease = this.conn.get(
        `SELECT lease_token FROM leases WHERE task_id = $taskId`,
        { $taskId: taskId },
      ) as { lease_token: string } | null;
      if (!lease || lease.lease_token !== opts.leaseToken) {
        throw new LeaseLostError(
          `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
          { taskId, leaseToken: opts.leaseToken },
        );
      }
      const lastRow = this.conn.get(
        `SELECT MAX(seq) as maxSeq FROM task_events WHERE task_id = $taskId`,
        { $taskId: taskId },
      ) as { maxSeq: number | null };
      let last = lastRow.maxSeq ?? -1;
      // Validate the whole batch before writing anything.
      for (const event of events) {
        if (event.seq <= last) {
          throw new SeqConflictError(
            `Non-monotonic seq ${event.seq} for task ${taskId} (last ${last}).`,
            { taskId, seq: event.seq, last },
          );
        }
        last = event.seq;
      }
      for (const event of events) {
        try {
          this.conn.run(
            `INSERT INTO task_events (task_id, seq, event_id, attempt_id, type, timestamp, payload)
             VALUES ($taskId, $seq, $eventId, $attemptId, $type, $timestamp, $payload)`,
            {
              $taskId: taskId,
              $seq: event.seq,
              $eventId: event.eventId,
              $attemptId: event.attemptId ?? null,
              $type: event.type,
              $timestamp: event.timestamp,
              $payload: toJson(event),
            },
          );
        } catch (err) {
          if (isConstraintError(err)) {
            throw new SeqConflictError(
              `Duplicate seq or eventId for task ${taskId} (seq ${event.seq}).`,
              { taskId, seq: event.seq, cause: String(err) },
            );
          }
          throw err;
        }
      }
    }, this.txOwner);
  }

  async listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]> {
    let sql = `SELECT * FROM task_events WHERE task_id = $taskId`;
    const params: Params = { $taskId: taskId };
    if (opts?.afterSeq !== undefined) {
      sql += ` AND seq > $after`;
      params.$after = opts.afterSeq;
    }
    sql += ` ORDER BY seq ASC`;
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as TaskEventRow[];
    return rows.map((row) => parseJson<TaskEventEnvelope>(row.payload));
  }

  async nextSeq(taskId: string): Promise<number> {
    const row = this.conn.get(
      `SELECT MAX(seq) as maxSeq FROM task_events WHERE task_id = $taskId`,
      { $taskId: taskId },
    ) as { maxSeq: number | null };
    return (row.maxSeq ?? -1) + 1;
  }

  async updateProgress(
    taskId: string,
    progress: Record<string, unknown>,
    opts: UpdateProgressOptions,
  ): Promise<TaskRecord> {
    return this.conn.whenFree(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      // The same ownership proof `appendEvents` demands, read inside the same
      // transaction as the write it guards.
      const lease = this.conn.get(
        `SELECT lease_token FROM leases WHERE task_id = $taskId`,
        { $taskId: taskId },
      ) as { lease_token: string } | null;
      if (!lease || lease.lease_token !== opts.leaseToken) {
        throw new LeaseLostError(
          `Lease token ${opts.leaseToken} is not current for task ${taskId}.`,
          { taskId, leaseToken: opts.leaseToken },
        );
      }
      // Plain assignment, not COALESCE: progress is an overwritten snapshot,
      // and the whole shape belongs to the latest writer.
      this.conn.run(
        `UPDATE tasks SET progress = $progress WHERE task_id = $id`,
        {
          $progress: toJson(progress),
          $id: taskId,
        },
      );
      return taskFromRow(this.selectTaskRow(taskId)!);
    }, this.txOwner);
  }

  /**
   * A claim is one transaction of its own: task row, attempt and lease land
   * together or not at all.
   *
   * OF ITS OWN is the load-bearing part, and it is the connection's FIFO that
   * provides it (see {@link SqliteConnection.withAsyncTx}). `claimNext` awaits
   * inside its candidate walk, so overlapping calls used to flatten into ONE
   * transaction and make the second caller's grant hostage to the first: a
   * rollback anywhere on that shared path reverted the task row to `queued`
   * while the attempt and lease written afterwards committed, and a later
   * `claimNext` handed the same task to a second worker. The same happened to a
   * claim that arrived while an unrelated `AssistantStore.transaction` was
   * open.
   *
   * A claim issued through the `tx` view of an open transaction is the one
   * caller that still joins it — {@link txOwner} is set on that copy, and such
   * a caller asked for one unit of work.
   */
  async claimNext(input: ClaimNextInput): Promise<ClaimedTask | null> {
    // `owner` is the transaction THIS call opened (or, on the flattened path,
    // the caller's). Every write below is made through it, because they belong
    // to the claim's own unit of work — a `whenFree` gated on `this.txOwner`
    // would wait for the transaction it is already running inside.
    return this.conn.withAsyncTx(async (owner) => {
      const nowIso = input.now.toISOString();
      const rows = this.selectClaimCandidates(
        nowIso,
        input.scopesBusy,
        input.kinds,
      );
      // Walk the ordered candidates rather than taking the first: the head of
      // the queue can be gated on a dependency still in flight, or doomed by
      // one that failed, and neither may hide the claimable work behind it.
      //
      // The rows are a SNAPSHOT. The connection's gate keeps two `claimNext`
      // calls apart, but nothing stops another caller settling or claiming
      // one of these tasks between the SELECT and this row's turn — so a lost
      // `queued`-> CAS means someone else got there first, which is the race
      // resolving normally, not a fault: skip the row and keep walking.
      for (const row of rows) {
        const verdict = evaluateTaskDependencies(this.dependencyStates(row));
        if (verdict.kind === "blocked") continue;
        if (verdict.kind === "settle") {
          // Settled here, on the claim path, instead of by a background sweep
          // — see TaskStore.claimNext. The scan then continues past it.
          try {
            await this.transitionTaskAs(
              owner,
              row.task_id,
              ["queued"],
              verdict.to,
              {
                finishedAt: this.clock.nowIso(),
                ...(verdict.error === undefined
                  ? {}
                  : { error: verdict.error }),
              },
            );
          } catch (err) {
            if (!(err instanceof InvalidTaskTransitionError)) throw err;
          }
          continue;
        }
        let task: TaskRecord;
        try {
          task = await this.transitionTaskAs(
            owner,
            row.task_id,
            ["queued"],
            "running",
            { startedAt: this.clock.nowIso() },
          );
        } catch (err) {
          if (!(err instanceof InvalidTaskTransitionError)) throw err;
          continue;
        }
        const attempt = await this.createAttemptAs(owner, {
          attemptId: this.ids.attemptId(),
          taskId: task.taskId,
          ownerId: input.ownerId,
        });
        const lease = await this.acquireLeaseAs(owner, {
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: input.ownerId,
          ttlMs: this.leaseTtlMs,
        });
        return { task, attempt, lease };
      }
      return null;
    }, this.txOwner);
  }

  async markDeadLettered(
    taskId: string,
    reason: string,
    opts?: FencedWriteOptions,
  ): Promise<TaskRecord> {
    return this.conn.whenFree(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      if (opts?.leaseToken !== undefined) {
        this.assertLeaseCurrent(taskId, opts.leaseToken);
      }
      const at = this.clock.nowIso();
      this.conn.run(
        `UPDATE tasks SET dead_lettered_at = $at, dead_letter_reason = $reason WHERE task_id = $id`,
        { $at: at, $reason: reason, $id: taskId },
      );
      return taskFromRow(this.selectTaskRow(taskId)!);
    }, this.txOwner);
  }

  /**
   * Refuse a write whose `leaseToken` is not the task's CURRENT lease.
   *
   * READ INSIDE THE CALLER'S TRANSACTION, which is the entire point: a runner
   * can only check ownership and then write across two awaits, and the gap is
   * where a zombie attempt lands a verdict over the live one's. Here the proof
   * and the write cannot be separated.
   *
   * The lease table is one row per task (PK on `task_id`, replaced by every
   * `acquireLease`), so the row this reads always carries the HIGHEST fencing
   * token ever issued for the task — matching the token therefore IS the
   * fencing comparison, with no second value to compare. The token is reported
   * in `details` so an operator can tell "your generation was superseded" from
   * "there is no lease at all".
   */
  private assertLeaseCurrent(taskId: string, leaseToken: string): void {
    const lease = this.conn.get(
      `SELECT lease_token, fencing_token FROM leases WHERE task_id = $taskId`,
      { $taskId: taskId },
    ) as { lease_token: string; fencing_token: number } | null;
    if (!lease || lease.lease_token !== leaseToken) {
      throw new LeaseLostError(
        `Lease token ${leaseToken} is not current for task ${taskId}.`,
        {
          taskId,
          leaseToken,
          ...(lease === null
            ? {}
            : { currentFencingToken: lease.fencing_token }),
        },
      );
    }
  }

  private selectTaskRow(taskId: string): TaskRow | null {
    return (
      (this.conn.get(`SELECT * FROM tasks WHERE task_id = $id`, {
        $id: taskId,
      }) as TaskRow | undefined) ?? null
    );
  }

  /**
   * The claim query: status/availableAt/scope/kind filters, ordered by
   * effective priority desc, then `enqueued_at` asc, then `rowid` asc as a
   * final deterministic tie-break (insertion order, for when two rows share a
   * millisecond timestamp).
   *
   * Effective priority is `priority + min(maxBonus, floor(waitMs / intervalMs)
   * * bonus)` — the formula in `@agentkit/host`'s `ports/task-aging.ts`,
   * expressed in SQL so the
   * ORDER BY sees it rather than the caller re-sorting a page of rows that was
   * already chosen by the wrong key. With the default `bonus = 0` the term
   * folds to zero and the ordering is plain `priority DESC, enqueued_at ASC`.
   * The wait is computed via `julianday` (days as a float) rather than
   * `strftime('%s')` (whole seconds), because an aging interval shorter than a
   * second is otherwise silently rounded to "no wait at all".
   *
   * RETURNS EVERY CANDIDATE, ordered, not just the first — `claimNext` has to
   * be able to walk past a task its dependencies are still gating. That is a
   * full read of the claimable set for this worker, which is the honest cost of
   * doing dependency gating outside SQL in a reference adapter; a store that
   * expected a very deep queue would push the gate into the query (a
   * `depends_on` edge table with a NOT EXISTS correlated subquery) instead.
   *
   * `$now` is the CALLER-SUPPLIED `ClaimNextInput.now`, bound once and reused
   * in both the WHERE filter and the aging expression — not SQL's own
   * `strftime('%s','now')`. Reading wall-clock independently there would
   * disagree with the `available_at <= $now` filter by however long the
   * query takes to reach that clause, and would make the aging term
   * untestable (a caller cannot advance the database engine's clock, but
   * freely controls what `now` it passes in).
   *
   * An EMPTY `kinds` array means "no kind is acceptable", not "any kind": a
   * worker with an empty executor registry can claim nothing, and quietly
   * treating that as unfiltered would hand it work it cannot run.
   */
  private selectClaimCandidates(
    nowIso: string,
    scopesBusy: string[],
    kinds: string[] | undefined,
  ): TaskRow[] {
    let sql = `SELECT * FROM tasks WHERE status = 'queued' AND available_at <= $now`;
    const params: Params = {
      $now: nowIso,
      $agingIntervalMs: this.aging.intervalMs,
      $agingBonus: this.aging.bonus,
      $agingMaxBonus: this.aging.maxBonus,
    };
    if (scopesBusy.length > 0) {
      const placeholders = scopesBusy.map((_, i) => `$busy${i}`).join(", ");
      sql += ` AND scope_id NOT IN (${placeholders})`;
      scopesBusy.forEach((scope, i) => {
        params[`$busy${i}`] = scope;
      });
    }
    if (kinds !== undefined) {
      if (kinds.length === 0) return [];
      const placeholders = kinds.map((_, i) => `$kind${i}`).join(", ");
      sql += ` AND kind IN (${placeholders})`;
      kinds.forEach((kind, i) => {
        params[`$kind${i}`] = kind;
      });
    }
    sql += ` ORDER BY (priority + MIN($agingMaxBonus,
               CAST(MAX(0, (julianday($now) - julianday(enqueued_at)) * 86400000.0)
                    / $agingIntervalMs AS INTEGER) * $agingBonus)) DESC,
             enqueued_at ASC, rowid ASC`;
    return this.conn.all(sql, params) as TaskRow[];
  }

  /** The narrow projection {@link evaluateTaskDependencies} grades. */
  private dependencyStates(row: TaskRow): TaskDependencyState[] {
    if (row.depends_on === null) return [];
    return parseJson<string[]>(row.depends_on).map((dependencyId) => {
      const dependency = this.conn.get(
        `SELECT status, dead_lettered_at FROM tasks WHERE task_id = $id`,
        { $id: dependencyId },
      ) as { status: string; dead_lettered_at: string | null } | null;
      return {
        taskId: dependencyId,
        status: dependency === null ? null : (dependency.status as TaskStatus),
        deadLettered:
          dependency !== null && dependency.dead_lettered_at !== null,
      };
    });
  }

  /**
   * Prove every edge a new task declares points at a row that already exists —
   * the acyclicity guarantee, enforced at write time. See
   * {@link UnknownDependencyError}.
   */
  private assertDependenciesExist(
    taskId: string,
    parentTaskId: string | undefined,
    dependsOn: string[] | null,
  ): void {
    if (
      parentTaskId !== undefined &&
      this.selectTaskRow(parentTaskId) === null
    ) {
      throw new UnknownDependencyError(
        `Task ${taskId} names parent ${parentTaskId}, which does not exist.`,
        { taskId, parentTaskId },
      );
    }
    for (const dependency of dependsOn ?? []) {
      // Self-dependency first and by identity: the row is not written yet, so
      // a plain existence check would report the wrong reason for it.
      if (dependency === taskId || this.selectTaskRow(dependency) === null) {
        throw new UnknownDependencyError(
          `Task ${taskId} depends on ${dependency}, which does not exist.`,
          { taskId, dependsOn: dependency },
        );
      }
    }
  }
}

// ---------------------------------------------------------------------------
// ProposalStore
// ---------------------------------------------------------------------------

class SqliteProposalStore implements ProposalStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async create(input: CreateProposalInput): Promise<ProposalRecord> {
    return this.conn.whenFree(() => {
      try {
        this.conn.run(
          `INSERT INTO proposals
             (id, chat_id, run_id, scope_key, action_id, tool_name, kind, risk, status,
              envelope, operations, warnings, truncated, revision_at_create, created_at)
           VALUES
             ($id, $chatId, $runId, $scopeKey, $actionId, $toolName, $kind, $risk, 'pending',
              $envelope, $operations, $warnings, $truncated, $revisionAtCreate, $createdAt)`,
          {
            $id: input.id,
            $chatId: input.chatId,
            $runId: input.runId ?? null,
            $scopeKey: input.scopeKey,
            $actionId: input.actionId ?? null,
            $toolName: input.toolName,
            $kind: input.kind,
            $risk: input.risk,
            $envelope: toJson(input.envelope),
            $operations: toJson(input.operations),
            $warnings: toJson(input.warnings),
            $truncated: toIntBool(input.truncated),
            $revisionAtCreate: input.revisionAtCreate ?? null,
            $createdAt: input.createdAt,
          },
        );
      } catch (err) {
        if (isConstraintError(err)) {
          throw new DuplicateActionIdError(
            `action_id ${input.actionId} already used in scope ${input.scopeKey}.`,
            { scopeKey: input.scopeKey, actionId: input.actionId },
          );
        }
        throw err;
      }
      return proposalFromRow(this.selectProposalRow(input.id)!);
    }, this.txOwner);
  }

  async get(proposalId: string): Promise<ProposalRecord | null> {
    const row = this.selectProposalRow(proposalId);
    return row ? proposalFromRow(row) : null;
  }

  async getByActionId(
    scopeKey: string,
    actionId: string,
  ): Promise<ProposalRecord | null> {
    // Most recent wins: rowid increases with insertion order.
    const row = this.conn.get(
      `SELECT * FROM proposals WHERE scope_key = $scopeKey AND action_id = $actionId
       ORDER BY rowid DESC LIMIT 1`,
      { $scopeKey: scopeKey, $actionId: actionId },
    ) as ProposalRow | undefined;
    return row ? proposalFromRow(row) : null;
  }

  async listByChat(
    chatId: string,
    opts?: ListProposalsOptions,
  ): Promise<ProposalRecord[]> {
    let sql = `SELECT * FROM proposals WHERE chat_id = $chatId`;
    const params: Params = { $chatId: chatId };
    if (opts?.status !== undefined) {
      sql += ` AND status = $status`;
      params.$status = opts.status;
    }
    sql += ` ORDER BY rowid ASC`;
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as ProposalRow[];
    return rows.map(proposalFromRow);
  }

  async listByStatus(
    status: ProposalStatus,
    opts?: { limit?: number },
  ): Promise<ProposalRecord[]> {
    let sql = `SELECT * FROM proposals WHERE status = $status ORDER BY rowid ASC`;
    const params: Params = { $status: status };
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as ProposalRow[];
    return rows.map(proposalFromRow);
  }

  async transition(
    proposalId: string,
    from: ProposalStatus[],
    to: ProposalStatus,
    patch?: ProposalPatch,
  ): Promise<ProposalRecord> {
    return this.conn.whenFree(() => {
      const row = this.selectProposalRow(proposalId);
      if (!row)
        throw new RecordNotFoundError(`Proposal not found: ${proposalId}`);
      const current = row.status as ProposalStatus;
      if (!from.includes(current)) {
        throw new InvalidProposalTransitionError(
          `Proposal ${proposalId} is ${current}, expected one of [${from.join(", ")}].`,
          { proposalId, current, from, to },
        );
      }
      assertProposalTransition(current, to);
      const result = this.conn.run(
        `UPDATE proposals SET
           status = $status,
           decision = COALESCE($decision, decision),
           decided_at = COALESCE($decidedAt, decided_at),
           applied_at = COALESCE($appliedAt, applied_at),
           operation_id = COALESCE($operationId, operation_id),
           reason = COALESCE($reason, reason)
         WHERE id = $id AND status = $current`,
        {
          $status: to,
          $decision:
            patch?.decision !== undefined ? toJson(patch.decision) : null,
          $decidedAt: patch?.decidedAt ?? null,
          $appliedAt: patch?.appliedAt ?? null,
          $operationId: patch?.operationId ?? null,
          $reason: patch?.reason ?? null,
          $id: proposalId,
          $current: current,
        },
      );
      if (result.changes === 0) {
        throw new InvalidProposalTransitionError(
          `Proposal ${proposalId} changed concurrently; expected one of [${from.join(", ")}].`,
          { proposalId, from, to },
        );
      }
      return proposalFromRow(this.selectProposalRow(proposalId)!);
    }, this.txOwner);
  }

  async recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome> {
    return this.conn.whenFree(() => {
      const existing = this.conn.get(
        `SELECT * FROM proposal_outcomes WHERE operation_id = $id`,
        { $id: operationId },
      ) as ProposalOutcomeRow | undefined;
      // Idempotent on operationId: the first outcome is the one that
      // happened; a later call must not overwrite the evidence.
      if (existing) return outcomeFromRow(existing);
      this.conn.run(
        `INSERT INTO proposal_outcomes (operation_id, status, applied_ops, failed_ops, result_json, revision)
         VALUES ($id, $status, $appliedOps, $failedOps, $resultJson, $revision)`,
        {
          $id: operationId,
          $status: outcome.status,
          $appliedOps: outcome.appliedOps,
          $failedOps: toJson(outcome.failedOps),
          $resultJson: outcome.resultJson ?? null,
          $revision: outcome.revision ?? null,
        },
      );
      return outcome;
    }, this.txOwner);
  }

  async getOutcome(operationId: string): Promise<ApplyOutcome | null> {
    const row = this.conn.get(
      `SELECT * FROM proposal_outcomes WHERE operation_id = $id`,
      { $id: operationId },
    ) as ProposalOutcomeRow | undefined;
    return row ? outcomeFromRow(row) : null;
  }

  async invalidatePendingForRevision(
    scopeKey: string,
    newRevision: string,
  ): Promise<number> {
    return this.conn.whenFree(() => {
      const rows = this.conn.all(
        `SELECT * FROM proposals WHERE scope_key = $scopeKey AND status = 'pending'
           AND (revision_at_create IS NULL OR revision_at_create != $newRevision)`,
        { $scopeKey: scopeKey, $newRevision: newRevision },
      ) as ProposalRow[];
      const at = this.clock.nowIso();
      for (const row of rows) {
        assertProposalTransition("pending", "invalidated");
        this.conn.run(
          `UPDATE proposals SET status = 'invalidated', reason = 'revision_conflict', decided_at = $at
           WHERE id = $id AND status = 'pending'`,
          { $at: at, $id: row.id },
        );
      }
      return rows.length;
    }, this.txOwner);
  }

  /**
   * Delete a chat's proposals and the outcomes they claimed, in ONE
   * transaction.
   *
   * BY `chat_id`, never by `scope_key` — see the port: two chats routinely
   * propose writes into one shared scope, and deleting the scope would take a
   * bystander's staged writes with it.
   *
   * Outcomes first: they are keyed by `operation_id`, which only the proposal
   * row still names, so deleting the proposals first would leave rows nothing
   * can ever identify again.
   */
  async deleteByChat(chatId: string): Promise<number> {
    return this.conn.whenFree(() => {
      const params: Params = { $chatId: chatId };
      this.conn.run(
        `DELETE FROM proposal_outcomes WHERE operation_id IN (
           SELECT operation_id FROM proposals
            WHERE chat_id = $chatId AND operation_id IS NOT NULL)`,
        params,
      );
      return this.conn.run(
        `DELETE FROM proposals WHERE chat_id = $chatId`,
        params,
      ).changes;
    }, this.txOwner);
  }

  private selectProposalRow(proposalId: string): ProposalRow | null {
    return (
      (this.conn.get(`SELECT * FROM proposals WHERE id = $id`, {
        $id: proposalId,
      }) as ProposalRow | undefined) ?? null
    );
  }
}

// ---------------------------------------------------------------------------
// ProviderStore / SettingsStore / OutboxStore
// ---------------------------------------------------------------------------

class SqliteProviderStore implements ProviderStore {
  constructor(
    private readonly conn: SqliteConnection,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async listProviders(): Promise<AiProviderConfig[]> {
    const rows = this.conn.all(
      `SELECT * FROM providers ORDER BY rowid ASC`,
    ) as ProviderRow[];
    return rows.map(providerFromRow);
  }

  async getProvider(providerId: string): Promise<AiProviderConfig | null> {
    const row = this.conn.get(`SELECT * FROM providers WHERE id = $id`, {
      $id: providerId,
    }) as ProviderRow | undefined;
    return row ? providerFromRow(row) : null;
  }

  async upsertProvider(config: AiProviderConfig): Promise<AiProviderConfig> {
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO providers (id, label, kind, base_url, api_key, default_model, enabled, extra_headers, metadata)
         VALUES ($id, $label, $kind, $baseUrl, $apiKey, $defaultModel, $enabled, $extraHeaders, $metadata)
         ON CONFLICT(id) DO UPDATE SET
           label = excluded.label, kind = excluded.kind, base_url = excluded.base_url,
           api_key = excluded.api_key, default_model = excluded.default_model, enabled = excluded.enabled,
           extra_headers = excluded.extra_headers, metadata = excluded.metadata`,
        {
          $id: config.id,
          $label: config.label,
          $kind: config.kind,
          $baseUrl: config.baseUrl,
          $apiKey: config.apiKey ?? null,
          $defaultModel: config.defaultModel,
          $enabled: toIntBool(config.enabled),
          $extraHeaders:
            config.extraHeaders === undefined
              ? null
              : toJson(config.extraHeaders),
          $metadata:
            config.metadata === undefined ? null : toJson(config.metadata),
        },
      );
    }, this.txOwner);
    return config;
  }

  async deleteProvider(providerId: string): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(`DELETE FROM providers WHERE id = $id`, {
        $id: providerId,
      });
      this.conn.run(`DELETE FROM provider_models WHERE provider_id = $id`, {
        $id: providerId,
      });
      this.conn.run(
        `DELETE FROM provider_capabilities WHERE provider_id = $id`,
        {
          $id: providerId,
        },
      );
    }, this.txOwner);
  }

  async listModels(providerId: string): Promise<AiProviderModel[]> {
    const rows = this.conn.all(
      `SELECT * FROM provider_models WHERE provider_id = $id ORDER BY model_id ASC`,
      { $id: providerId },
    ) as ProviderModelRow[];
    return rows.map(modelFromRow);
  }

  async replaceModels(
    providerId: string,
    models: AiProviderModel[],
  ): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(`DELETE FROM provider_models WHERE provider_id = $id`, {
        $id: providerId,
      });
      for (const model of models) {
        this.conn.run(
          `INSERT INTO provider_models
             (provider_id, model_id, display_name, context_window_tokens, supports_tool_calling, fetched_at)
           VALUES ($providerId, $modelId, $displayName, $contextWindowTokens, $supportsToolCalling, $fetchedAt)`,
          {
            $providerId: providerId,
            $modelId: model.modelId,
            $displayName: model.displayName,
            $contextWindowTokens: model.contextWindowTokens ?? null,
            $supportsToolCalling: toOptionalIntBool(model.supportsToolCalling),
            $fetchedAt: model.fetchedAt,
          },
        );
      }
    }, this.txOwner);
  }

  async getCapabilities(
    providerId: string,
  ): Promise<AiProviderCapabilities | null> {
    const row = this.conn.get(
      `SELECT * FROM provider_capabilities WHERE provider_id = $id`,
      { $id: providerId },
    ) as ProviderCapabilitiesRow | undefined;
    return row ? capabilitiesFromRow(row) : null;
  }

  async saveCapabilities(
    providerId: string,
    capabilities: AiProviderCapabilities,
  ): Promise<void> {
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO provider_capabilities
           (provider_id, streaming, tool_calling, model_list, vision, json_mode, max_context_tokens, checked_at, warning)
         VALUES ($id, $streaming, $toolCalling, $modelList, $vision, $jsonMode, $maxContextTokens, $checkedAt, $warning)
         ON CONFLICT(provider_id) DO UPDATE SET
           streaming = excluded.streaming, tool_calling = excluded.tool_calling,
           model_list = excluded.model_list, vision = excluded.vision, json_mode = excluded.json_mode,
           max_context_tokens = excluded.max_context_tokens, checked_at = excluded.checked_at, warning = excluded.warning`,
        {
          $id: providerId,
          $streaming: toIntBool(capabilities.streaming),
          $toolCalling: toIntBool(capabilities.toolCalling),
          $modelList: toIntBool(capabilities.modelList),
          $vision: toOptionalIntBool(capabilities.vision),
          $jsonMode: toOptionalIntBool(capabilities.jsonMode),
          $maxContextTokens: capabilities.maxContextTokens ?? null,
          $checkedAt: capabilities.checkedAt ?? null,
          $warning: capabilities.warning ?? null,
        },
      );
    }, this.txOwner);
  }
}

class SqliteSettingsStore implements SettingsStore {
  constructor(
    private readonly conn: SqliteConnection,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async getSettings(): Promise<AssistantSettings> {
    const row = this.conn.get(
      `SELECT * FROM settings WHERE id = 1`,
    ) as SettingsRow;
    return settingsFromRow(row);
  }

  async updateSettings(
    patch: Partial<AssistantSettings>,
  ): Promise<AssistantSettings> {
    return this.conn.whenFree(() => {
      const row = this.conn.get(
        `SELECT * FROM settings WHERE id = 1`,
      ) as SettingsRow;
      const merged: AssistantSettings = { ...settingsFromRow(row), ...patch };
      this.conn.run(
        `UPDATE settings SET
           default_provider_id = $defaultProviderId, default_model = $defaultModel,
           context_size_preference = $contextSizePreference, write_policy_mode = $writePolicyMode,
           allow_raw_tool_data = $allowRawToolData, max_tool_iterations = $maxToolIterations,
           tool_calling_mode = $toolCallingMode, metadata = $metadata
         WHERE id = 1`,
        {
          $defaultProviderId: merged.defaultProviderId ?? null,
          $defaultModel: merged.defaultModel ?? null,
          $contextSizePreference: merged.contextSizePreference,
          $writePolicyMode: merged.writePolicyMode,
          $allowRawToolData: toIntBool(merged.allowRawToolData),
          $maxToolIterations: merged.maxToolIterations ?? null,
          // The column is NOT NULL: an explicit `undefined` in the patch means
          // "back to the default", not "store nothing".
          $toolCallingMode: merged.toolCalling ?? DEFAULT_TOOL_CALLING_MODE,
          $metadata: toJson(merged.metadata),
        },
      );
      return merged;
    }, this.txOwner);
  }
}

class SqliteOutboxStore implements OutboxStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly claimVisibilityMs: number = DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS,
    private readonly maxAttempts: number = DEFAULT_OUTBOX_MAX_ATTEMPTS,
    /**
     * Set only on the copy {@link SqliteAssistantStore.transaction} hands its
     * callback: the identity of that transaction, so writes made through it
     * join the caller's unit of work instead of queueing behind it. See
     * {@link SqliteConnection.whenFree}.
     */
    private readonly txOwner?: TxOwner,
  ) {}

  async enqueue(input: OutboxAppendInput): Promise<OutboxRecord> {
    const now = this.clock.nowIso();
    const id = input.id ?? `outbox_${crypto.randomUUID()}`;
    // Normalized because `claimBatch` compares this column as TEXT — see
    // `normalizeInstant`.
    const availableAt =
      input.availableAt === undefined
        ? now
        : normalizeInstant(input.availableAt, "availableAt");
    await this.conn.whenFree(() => {
      this.conn.run(
        `INSERT INTO outbox (id, topic, run_id, payload, created_at, available_at, attempts)
         VALUES ($id, $topic, $runId, $payload, $now, $availableAt, 0)`,
        {
          $id: id,
          $topic: input.topic,
          $runId: input.runId ?? null,
          $payload: toJson(input.payload),
          $now: now,
          $availableAt: availableAt,
        },
      );
    }, this.txOwner);
    return {
      id,
      topic: input.topic,
      ...(input.runId === undefined ? {} : { runId: input.runId }),
      payload: input.payload,
      createdAt: now,
      availableAt,
      attempts: 0,
    };
  }

  async claimBatch(input: OutboxClaimInput): Promise<OutboxRecord[]> {
    return this.conn.whenFree(() => {
      const nowIso = input.now.toISOString();
      // `attempts < $maxAttempts` is the cap, and it needs no column of its
      // own: `attempts` already counts deliveries, so a record that has used
      // its budget simply stops matching the claim query and stays behind as an
      // inspectable dead letter.
      const rows = this.conn.all(
        `SELECT * FROM outbox
          WHERE published_at IS NULL AND available_at <= $now AND attempts < $maxAttempts
          ORDER BY available_at ASC, rowid ASC LIMIT $limit`,
        { $now: nowIso, $limit: input.limit, $maxAttempts: this.maxAttempts },
      ) as OutboxRow[];
      if (rows.length === 0) return [];
      // Push the visibility window forward so a concurrent claimBatch call
      // does not hand the same in-flight record to a second publisher before
      // markPublished/markFailed resolves it — the port has no separate
      // "claimed" flag, so available_at doubles as the claim lease.
      const newAvailableAt = new Date(
        input.now.getTime() + this.claimVisibilityMs,
      ).toISOString();
      const claimed: OutboxRecord[] = [];
      for (const row of rows) {
        const attempts = row.attempts + 1;
        this.conn.run(
          `UPDATE outbox SET attempts = $attempts, available_at = $availableAt WHERE id = $id`,
          { $attempts: attempts, $availableAt: newAvailableAt, $id: row.id },
        );
        claimed.push(
          outboxFromRow({ ...row, attempts, available_at: newAvailableAt }),
        );
      }
      return claimed;
    }, this.txOwner);
  }

  async markPublished(id: string, at: Date): Promise<void> {
    await this.conn.whenFree(() => {
      const result = this.conn.run(
        `UPDATE outbox SET published_at = $at WHERE id = $id`,
        { $at: at.toISOString(), $id: id },
      );
      assertOutboxRowTouched(result, id);
    }, this.txOwner);
  }

  async markFailed(id: string, error: string, retryAt: Date): Promise<void> {
    await this.conn.whenFree(() => {
      const result = this.conn.run(
        `UPDATE outbox SET last_error = $error, available_at = $retryAt WHERE id = $id`,
        { $error: error, $retryAt: retryAt.toISOString(), $id: id },
      );
      assertOutboxRowTouched(result, id);
    }, this.txOwner);
  }

  /**
   * Drop what can never be claimed again: published records older than
   * `before`, and records that used up their attempt budget before it.
   *
   * The two halves compare DIFFERENT columns on purpose. A published record is
   * retained for as long as someone might want to read what was sent, which is
   * measured from when it was sent (`published_at`); an exhausted one was never
   * sent at all, so the only age it has is its own (`created_at`).
   */
  async prune(before: Date): Promise<number> {
    return this.conn.whenFree(() => {
      const beforeIso = before.toISOString();
      return this.conn.run(
        `DELETE FROM outbox
          WHERE (published_at IS NOT NULL AND published_at < $before)
             OR (published_at IS NULL AND attempts >= $maxAttempts AND created_at < $before)`,
        { $before: beforeIso, $maxAttempts: this.maxAttempts },
      ).changes;
    }, this.txOwner);
  }
}

/**
 * A `markPublished`/`markFailed` that matched no row is a caller naming an id
 * this store does not have — a publisher resolving a record someone pruned, or
 * a plain typo. It used to be a silent no-op, which made "the publisher says it
 * published, the row says it did not" a mystery with no error anywhere.
 */
function assertOutboxRowTouched(result: Changes, id: string): void {
  if (result.changes === 0) {
    throw new RecordNotFoundError(`Outbox record not found: ${id}`, { id });
  }
}

/**
 * Refuse a database this build cannot read, instead of layering v2 tables over
 * v1 ones and discovering the mismatch at the first query.
 *
 * `PRAGMA user_version` is SQLite's own four-byte header slot — no bookkeeping
 * table, nothing to create before it can be read, and 0 on a database nobody
 * has stamped. A FRESH (or empty) file is stamped and initialized; anything
 * else carrying a different version is a hard error, because this adapter is
 * workspace-private and deliberately ships NO MIGRATIONS: a reference
 * implementation with half-tested upgrade scripts would be advertising a
 * durability guarantee it has not earned. Recreating the dev database is the
 * intended fix; a host that needs upgrades in place owns that with its own
 * store.
 */
function assertSchemaVersion(db: Database, path: string): void {
  const version = (
    db.query(`PRAGMA user_version`).get() as { user_version: number } | null
  )?.user_version;
  if (version === SCHEMA_VERSION) return;
  if (version === undefined) {
    // A pragma every SQLite build answers came back with nothing. Whatever this
    // handle is, it is not a database this adapter can reason about — and the
    // one thing worse than refusing to open it is opening it anyway and running
    // `SCHEMA_V7` against it, which is exactly what falling through would do.
    throw new AgentKitHostError(
      "sqlite_schema_version",
      `Cannot read user_version from the SQLite store at ${path}; refusing to touch this database.`,
      { path, expected: SCHEMA_VERSION },
    );
  }
  // An unstamped database with no tables is a fresh file (or an older build's
  // empty scratch db): there is nothing to preserve, so stamping it is safe.
  const tables = (
    db
      .query(
        `SELECT COUNT(*) as count FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'`,
      )
      .get() as { count: number }
  ).count;
  if (version === 0 && tables === 0) return;
  throw new AgentKitHostError(
    "sqlite_schema_version",
    `SQLite store at ${path} is schema version ${version}, but this build expects ${SCHEMA_VERSION}. ` +
      `This workspace-private reference adapter ships no migrations — delete and recreate the dev database.`,
    { path, found: version, expected: SCHEMA_VERSION },
  );
}

/**
 * Open (or adopt) a database file carrying THIS build's schema, with the
 * pragmas every store over it depends on.
 *
 * Factored out of {@link SqliteAssistantStore}'s constructor because it is no
 * longer the only store over this file: {@link SqliteMcpServerConfigStore} is
 * constructible from a path too, and a second hand-written copy of the
 * open-assert-apply-stamp sequence is how one of them ends up skipping the
 * version check on the day the sequence changes.
 *
 * Applying `SCHEMA_V7` unconditionally is safe by construction — every
 * statement in it is `CREATE ... IF NOT EXISTS` or `INSERT OR IGNORE` — so
 * opening a file this build (or another process running it) already
 * initialized is a no-op.
 *
 * ONE TRANSACTION FOR THE VERSION CHECK, THE DDL AND THE FTS BACKFILL. `exec`
 * of a multi-statement string runs each statement in its OWN implicit
 * transaction, and this sequence is a check-then-act twice over: the version
 * check decides whether to stamp `user_version`, and the DDL's trailing
 * backfill is guarded by `WHERE NOT EXISTS (SELECT 1 FROM
 * message_search_docsize)`. Two openers of one file — two processes, or one
 * process and a worker — are otherwise free to interleave inside that sequence
 * and both decide the index is empty, double-indexing every message in it.
 * `BEGIN IMMEDIATE` holds the write lock across the whole thing, so no
 * decision here can be separated from the act it authorises by somebody else's
 * commit. The same discipline `TaskStore.deleteByScope` follows for its busy
 * check, and for the same reason: a check-then-act is not atomic just because
 * each of its statements is.
 */
export function openAgentKitDatabase(
  path: string | ":memory:",
  busyTimeoutMs: number = DEFAULT_BUSY_TIMEOUT_MS,
): Database {
  const db = new Database(path);
  // Several handles over one file are supported; this is half of what makes
  // them wait for each other rather than fail on each other (the other half
  // is `SqliteConnection.beginImmediateAsync` — see that class's doc).
  //
  // FIRST, BEFORE ANY OTHER STATEMENT, and that ordering is load-bearing: it
  // used to be set after the journal-mode pragma below, which left that pragma
  // — a statement that takes an EXCLUSIVE lock — running with SQLite's default
  // busy handler, the one that gives up instantly. Measured: six processes
  // opening one file at once, and five of them died with a raw
  // `SQLiteError: database is locked` out of a function that documents no such
  // failure. A busy timeout that is set after the statement that needed it is
  // not a busy timeout.
  db.exec(`PRAGMA busy_timeout = ${busyTimeoutMs};`);
  if (path !== ":memory:") {
    // Outside the transaction below, and it has to be: SQLite refuses to change
    // the journal mode inside one.
    db.exec("PRAGMA journal_mode = WAL;");
  }
  db.exec("PRAGMA foreign_keys = ON;");
  db.exec("BEGIN IMMEDIATE");
  try {
    assertSchemaVersion(db, path);
    db.exec(SCHEMA_V7);
    db.exec(`PRAGMA user_version = ${SCHEMA_VERSION};`);
    db.exec("COMMIT");
  } catch (err) {
    try {
      db.exec("ROLLBACK");
    } catch {
      // Nothing open to roll back — the BEGIN is the only thing that could
      // have left one, and whatever ended it did so before this point.
    }
    throw err;
  }
  return db;
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/** {@link AssistantStore} minus `transaction` — the six sub-stores alone. */
type AssistantStoreStores = Omit<AssistantStore, "transaction">;

export interface SqliteAssistantStoreOptions extends TaskAgingOptions {
  /** Defaults to {@link defaultClock} (real wall-clock). */
  clock?: Clock;
  /** Defaults to {@link defaultIds} (UUID-backed). */
  ids?: IdGenerator;
  /** Lease TTL `claimNext` grants the attempt it creates. Default 30s. */
  leaseTtlMs?: number;
  /** Outbox claim-visibility window. Default 30s. */
  outboxClaimVisibilityMs?: number;
  /**
   * How many delivery attempts one outbox record gets before `claimBatch`
   * stops offering it. Default 10 — see {@link OutboxStore.claimBatch}.
   */
  outboxMaxAttempts?: number;
  /**
   * How long a transaction waits for the write lock before giving up, when
   * another connection on the same file holds it. Default 5s.
   *
   * Only meaningful for a file-backed store opened by more than one handle —
   * see the multi-handle section on {@link SqliteConnection}.
   */
  busyTimeoutMs?: number;
}

/**
 * bun:sqlite-backed, complete {@link AssistantStore}.
 *
 * `transaction(fn)` opens a real `BEGIN IMMEDIATE` and commits or rolls back
 * around `fn` — unlike `MemoryAssistantStore`, a throw inside `fn` discards
 * every write `fn` made. Nested `transaction()` calls on the `tx` it hands the
 * callback (including a port method that itself opens a mini-transaction, like
 * `transitionTask` or `createAttempt`) are FLATTENED into the outermost one
 * rather than nested — `bun:sqlite` has no savepoint support in this v1, so
 * re-entrant calls just run against the already-open transaction.
 *
 * WRITES ARE SERIALIZED PER CONNECTION: a second caller's `transaction()`, a
 * worker's `claimNext`, and every ordinary WRITE method issued while a
 * transaction is open all WAIT for it, and then run in a transaction of their
 * own. They used to join the open one and be rolled back by a stranger's throw.
 * Reads are exempt and still join, because they take no lock worth serializing.
 *
 * THE COROLLARY IS THAT A CALLBACK MUST DO ITS WORK THROUGH THE `tx` IT IS
 * GIVEN. A write made on the ROOT store from inside the callback is, by
 * construction, indistinguishable from an unrelated caller's, so awaiting one
 * in there waits on a transaction that cannot finish until the callback
 * returns. That was already true of a root-store `transaction()`/`claimNext`;
 * it is now true of `store.conversations.updateChat(...)` and every other
 * write. See {@link SqliteConnection}.
 */
export class SqliteAssistantStore implements AssistantStore {
  private readonly conn: SqliteConnection;
  /** Every sub-store, bound to one open transaction — see {@link txView}. */
  private readonly viewFor: (owner: TxOwner) => AssistantStoreStores;
  readonly conversations: ConversationStore;
  readonly tasks: TaskStore;
  readonly proposals: ProposalStore;
  readonly providers: ProviderStore;
  readonly settings: SettingsStore;
  readonly outbox: OutboxStore;

  constructor(
    path: string | ":memory:",
    options: SqliteAssistantStoreOptions = {},
  ) {
    const busyTimeoutMs = options.busyTimeoutMs ?? DEFAULT_BUSY_TIMEOUT_MS;
    const db = openAgentKitDatabase(path, busyTimeoutMs);
    this.conn = new SqliteConnection(db, busyTimeoutMs);
    const clock = options.clock ?? defaultClock;
    const ids = options.ids ?? defaultIds;
    // SECOND, IDENTICALLY-CONFIGURED INSTANCES rather than a mutable field on
    // the root ones: the token belongs to ONE transaction, and a field would
    // leak it to every other caller of `store.tasks` for as long as that
    // transaction is open — the exact confusion the token exists to end.
    const build = (owner?: TxOwner): AssistantStoreStores => ({
      conversations: new SqliteConversationStore(this.conn, clock, ids, owner),
      tasks: new SqliteTaskStore(
        this.conn,
        clock,
        ids,
        options.leaseTtlMs,
        options,
        owner,
      ),
      proposals: new SqliteProposalStore(this.conn, clock, owner),
      providers: new SqliteProviderStore(this.conn, owner),
      settings: new SqliteSettingsStore(this.conn, owner),
      outbox: new SqliteOutboxStore(
        this.conn,
        clock,
        options.outboxClaimVisibilityMs,
        options.outboxMaxAttempts,
        owner,
      ),
    });
    const root = build();
    this.conversations = root.conversations;
    this.tasks = root.tasks;
    this.proposals = root.proposals;
    this.providers = root.providers;
    this.settings = root.settings;
    this.outbox = root.outbox;
    this.viewFor = (owner) => build(owner);
  }

  async transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> {
    return this.conn.withAsyncTx((owner) => fn(this.txView(owner)));
  }

  /**
   * The aggregate as seen from INSIDE the transaction `owner` opened.
   *
   * The port already says `transaction` hands its callback "a store view scoped
   * to that transaction"; this is that view, and it is no longer `this` because
   * `this` carries no transaction identity. EVERY sub-store here is an
   * owner-bearing copy, not the root instance: a write now waits out a
   * transaction it does not own ({@link SqliteConnection.whenFree}), so a root
   * instance used in here would queue behind the very transaction it is running
   * inside and never finish. Carrying the owner is also what makes the
   * distinction meaningful in the other direction — `store.conversations` while
   * this transaction is open is a stranger's write, and waits.
   */
  private txView(owner: TxOwner): AssistantStore {
    const view = this.viewFor(owner);
    return {
      ...view,
      transaction: <T>(nested: (tx: AssistantStore) => Promise<T>) =>
        this.conn.withAsyncTx((nestedOwner) => {
          // `nestedOwner` is `owner` on the flattened path and a fresh token
          // only if this view outlived its transaction and had to open a new
          // one — either way the nested callback gets the view that matches the
          // transaction it is actually running in.
          return nested(this.txView(nestedOwner));
        }, owner),
    };
  }

  /**
   * The open handle, for a store over the SAME database that is not part of
   * this aggregate — {@link SqliteMcpServerConfigStore} is the one this exists
   * for.
   *
   * Sharing the handle rather than opening a second one is the point: one
   * connection means one write lock and one transaction depth, so a config
   * write issued while `transaction()` is open flattens into it instead of
   * deadlocking against it. A caller that takes this must NOT close it — the
   * aggregate owns the connection's lifetime, through {@link close}.
   */
  get database(): Database {
    return this.conn.db;
  }

  /** Closes the underlying connection. Safe to call once; further use throws. */
  close(): void {
    this.conn.db.close();
  }
}
