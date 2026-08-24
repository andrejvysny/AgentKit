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
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
  AiToolCall,
  TaskEventEnvelope,
} from "@agentkit/contracts";
import {
  AgentKitHostError,
  DuplicateActionIdError,
  DuplicateTaskError,
  InvalidProposalTransitionError,
  InvalidTaskTransitionError,
  LeaseLostError,
  RecordNotFoundError,
  SeqConflictError,
  assertProposalTransition,
  assertTaskTransition,
  defaultClock,
  defaultIds,
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
  type IdGenerator,
  type Lease,
  type ListChatsOptions,
  type ListEventsOptions,
  type ListMessagesOptions,
  type ListProposalsOptions,
  type MessageRecord,
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
  type TaskPatch,
  type TaskRecord,
  type TaskStatus,
  type TaskStore,
  type UpdateMessagePatch,
  type WritePolicyMode,
} from "@agentkit/host";
import { SCHEMA_V2, SCHEMA_VERSION } from "./schema.js";

const DEFAULT_LEASE_TTL_MS = 30_000;
const DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS = 30_000;

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
    code === "SQLITE_CONSTRAINT_UNIQUE" || code === "SQLITE_CONSTRAINT_PRIMARYKEY"
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
}
interface MessageRow {
  id: string;
  chat_id: string;
  run_id: string | null;
  role: string;
  content: string;
  order_key: number;
  tool_call_id: string | null;
  tool_calls: string | null;
  model_result_json: string | null;
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
  };
}

function messageFromRow(row: MessageRow): MessageRecord {
  return {
    id: row.id,
    chatId: row.chat_id,
    ...(row.run_id === null ? {} : { runId: row.run_id }),
    role: row.role as MessageRecord["role"],
    content: row.content,
    orderKey: row.order_key,
    ...(row.tool_call_id === null ? {} : { toolCallId: row.tool_call_id }),
    ...(row.tool_calls === null
      ? {}
      : { toolCalls: parseJson<AiToolCall[]>(row.tool_calls) }),
    ...(row.model_result_json === null
      ? {}
      : { modelResultJson: row.model_result_json }),
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
      : { supportsToolCalling: fromOptionalIntBool(row.supports_tool_calling) }),
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
 * {@link SqliteAssistantStore.transaction}), so a `txDepth` counter flattens
 * re-entrant calls: only the outermost `withTx`/`withAsyncTx` issues
 * BEGIN/COMMIT/ROLLBACK; anything nested inside it just runs against the
 * already-open transaction.
 */
class SqliteConnection {
  private txDepth = 0;

  constructor(readonly db: Database) {}

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

  get(sql: string, params?: Params): any {
    const stmt = this.db.query(sql);
    return params === undefined ? stmt.get() : stmt.get(params);
  }

  all(sql: string, params?: Params): any[] {
    const stmt = this.db.query(sql);
    return params === undefined ? stmt.all() : stmt.all(params);
  }

  exec(sql: string): void {
    this.db.exec(sql);
  }

  /** Synchronous transaction helper for a single port method's own SQL. */
  withTx<T>(fn: () => T): T {
    if (this.txDepth > 0) return fn();
    this.txDepth += 1;
    this.exec("BEGIN IMMEDIATE");
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
   * Async transaction helper for {@link AssistantStore.transaction}: `fn` may
   * itself `await` several port calls, each of which calls `withTx` and sees
   * `txDepth > 0`, flattening into this same outer transaction.
   */
  async withAsyncTx<T>(fn: () => Promise<T>): Promise<T> {
    if (this.txDepth > 0) return fn();
    this.txDepth += 1;
    this.exec("BEGIN IMMEDIATE");
    try {
      const result = await fn();
      this.exec("COMMIT");
      return result;
    } catch (err) {
      this.rollback();
      throw err;
    } finally {
      this.txDepth -= 1;
    }
  }

  private rollback(): void {
    try {
      this.exec("ROLLBACK");
    } catch {
      // Nothing to roll back — the failure happened before BEGIN took effect.
    }
  }
}

// ---------------------------------------------------------------------------
// ConversationStore
// ---------------------------------------------------------------------------

class SqliteConversationStore implements ConversationStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
  ) {}

  async createChat(input: CreateChatInput): Promise<ChatRecord> {
    const now = this.clock.nowIso();
    const id = input.id ?? `chat_${crypto.randomUUID()}`;
    const metadata = input.metadata ?? {};
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
    return {
      id,
      ...(input.title === undefined ? {} : { title: input.title }),
      createdAt: now,
      updatedAt: now,
      metadata,
    };
  }

  async getChat(chatId: string): Promise<ChatRecord | null> {
    const row = this.conn.get(`SELECT * FROM chats WHERE id = $id`, {
      $id: chatId,
    }) as ChatRow | null;
    return row ? chatFromRow(row) : null;
  }

  async listChats(opts?: ListChatsOptions): Promise<ChatRecord[]> {
    let sql = `SELECT * FROM chats`;
    const params: Params = {};
    if (opts?.before !== undefined) {
      sql += ` WHERE updated_at < $before`;
      params.$before = opts.before;
    }
    sql += ` ORDER BY updated_at DESC`;
    if (opts?.limit !== undefined) {
      sql += ` LIMIT $limit`;
      params.$limit = opts.limit;
    }
    const rows = this.conn.all(sql, params) as ChatRow[];
    return rows.map(chatFromRow);
  }

  async appendMessage(input: AppendMessageInput): Promise<MessageRecord> {
    return this.conn.withTx(() => {
      const chat = this.conn.get(`SELECT id FROM chats WHERE id = $id`, {
        $id: input.chatId,
      });
      if (!chat) {
        throw new RecordNotFoundError(`Chat not found: ${input.chatId}`);
      }
      const maxRow = this.conn.get(
        `SELECT COALESCE(MAX(order_key), 0) as maxKey FROM messages WHERE chat_id = $chatId`,
        { $chatId: input.chatId },
      ) as { maxKey: number };
      const orderKey = maxRow.maxKey + 1;
      const id = input.id ?? this.ids.messageId();
      const now = this.clock.nowIso();
      const metadata = input.metadata ?? {};
      this.conn.run(
        `INSERT INTO messages
           (id, chat_id, run_id, role, content, order_key, tool_call_id, tool_calls, model_result_json, metadata, created_at)
         VALUES
           ($id, $chatId, $runId, $role, $content, $orderKey, $toolCallId, $toolCalls, $modelResultJson, $metadata, $now)`,
        {
          $id: id,
          $chatId: input.chatId,
          $runId: input.runId ?? null,
          $role: input.role,
          $content: input.content,
          $orderKey: orderKey,
          $toolCallId: input.toolCallId ?? null,
          $toolCalls: input.toolCalls === undefined ? null : toJson(input.toolCalls),
          $modelResultJson: input.modelResultJson ?? null,
          $metadata: toJson(metadata),
          $now: now,
        },
      );
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
        ...(input.toolCallId === undefined ? {} : { toolCallId: input.toolCallId }),
        ...(input.toolCalls === undefined ? {} : { toolCalls: input.toolCalls }),
        ...(input.modelResultJson === undefined
          ? {}
          : { modelResultJson: input.modelResultJson }),
        metadata,
        createdAt: now,
      };
    });
  }

  async updateMessage(
    messageId: string,
    patch: UpdateMessagePatch,
  ): Promise<MessageRecord> {
    return this.conn.withTx(() => {
      const existing = this.conn.get(`SELECT * FROM messages WHERE id = $id`, {
        $id: messageId,
      }) as MessageRow | null;
      if (!existing) {
        throw new RecordNotFoundError(`Message not found: ${messageId}`);
      }
      const content = patch.content ?? existing.content;
      // Metadata REPLACES the stored bag, per the port contract.
      const metadataJson =
        patch.metadata !== undefined ? toJson(patch.metadata) : existing.metadata;
      const toolCallsJson =
        patch.toolCalls !== undefined
          ? toJson(patch.toolCalls)
          : existing.tool_calls;
      this.conn.run(
        `UPDATE messages SET content = $content, metadata = $metadata, tool_calls = $toolCalls WHERE id = $id`,
        {
          $content: content,
          $metadata: metadataJson,
          $toolCalls: toolCallsJson,
          $id: messageId,
        },
      );
      return messageFromRow({
        ...existing,
        content,
        metadata: metadataJson,
        tool_calls: toolCallsJson,
      });
    });
  }

  async listMessages(
    chatId: string,
    opts?: ListMessagesOptions,
  ): Promise<MessageRecord[]> {
    let sql = `SELECT * FROM messages WHERE chat_id = $chatId`;
    const params: Params = { $chatId: chatId };
    if (opts?.afterOrderKey !== undefined) {
      sql += ` AND order_key > $after`;
      params.$after = opts.afterOrderKey;
    }
    sql += ` ORDER BY order_key ASC`;
    const rows = this.conn.all(sql, params) as MessageRow[];
    const mapped = rows.map(messageFromRow);
    return opts?.limit !== undefined ? mapped.slice(-opts.limit) : mapped;
  }
}

// ---------------------------------------------------------------------------
// TaskStore
// ---------------------------------------------------------------------------

class SqliteTaskStore implements TaskStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly ids: IdGenerator,
    private readonly leaseTtlMs: number = DEFAULT_LEASE_TTL_MS,
  ) {}

  async createTask(input: CreateTaskInput): Promise<TaskRecord> {
    const now = this.clock.nowIso();
    const availableAt = input.availableAt ?? now;
    const priority = input.priority ?? 0;
    try {
      this.conn.run(
        `INSERT INTO tasks
           (task_id, kind, scope_id, status, priority, enqueued_at, available_at, payload, attempt_count, poison_count)
         VALUES
           ($taskId, $kind, $scopeId, 'queued', $priority, $now, $availableAt, $payload, 0, 0)`,
        {
          $taskId: input.taskId,
          $kind: input.kind,
          $scopeId: input.scopeId,
          $priority: priority,
          $now: now,
          $availableAt: availableAt,
          $payload: toJson(input.payload),
        },
      );
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
      attemptCount: 0,
      poisonCount: 0,
    };
  }

  async getTask(taskId: string): Promise<TaskRecord | null> {
    const row = this.selectTaskRow(taskId);
    return row ? taskFromRow(row) : null;
  }

  async transitionTask(
    taskId: string,
    from: TaskStatus[],
    to: TaskStatus,
    patch?: TaskPatch,
  ): Promise<TaskRecord> {
    return this.conn.withTx(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
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
          $availableAt: patch?.availableAt ?? null,
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
    });
  }

  async createAttempt(input: CreateAttemptInput): Promise<AttemptRecord> {
    return this.conn.withTx(() => {
      const task = this.selectTaskRow(input.taskId);
      if (!task) throw new RecordNotFoundError(`Task not found: ${input.taskId}`);
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
    });
  }

  async endAttempt(input: EndAttemptInput): Promise<AttemptRecord> {
    return this.conn.withTx(() => {
      const row = this.conn.get(
        `SELECT * FROM task_attempts WHERE attempt_id = $id`,
        { $id: input.attemptId },
      ) as AttemptRow | null;
      if (!row) {
        throw new RecordNotFoundError(`Attempt not found: ${input.attemptId}`);
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
    });
  }

  async acquireLease(input: AcquireLeaseInput): Promise<Lease> {
    return this.conn.withTx(() => {
      // Store-global monotonic fencing token, single-row counter table.
      this.conn.run(`UPDATE fencing_counter SET value = value + 1 WHERE id = 1`);
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
    });
  }

  async renewLease(leaseToken: string, ttlMs: number): Promise<Lease> {
    return this.conn.withTx(() => {
      const row = this.conn.get(`SELECT * FROM leases WHERE lease_token = $token`, {
        $token: leaseToken,
      }) as LeaseRow | null;
      if (!row) {
        throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
          leaseToken,
        });
      }
      const expiresAt = new Date(
        this.clock.now().getTime() + ttlMs,
      ).toISOString();
      this.conn.run(
        `UPDATE leases SET expires_at = $expiresAt WHERE lease_token = $token`,
        { $expiresAt: expiresAt, $token: leaseToken },
      );
      return leaseFromRow({ ...row, expires_at: expiresAt });
    });
  }

  async releaseLease(leaseToken: string): Promise<void> {
    this.conn.withTx(() => {
      const result = this.conn.run(`DELETE FROM leases WHERE lease_token = $token`, {
        $token: leaseToken,
      });
      if (result.changes === 0) {
        throw new LeaseLostError(`Lease token ${leaseToken} is not current.`, {
          leaseToken,
        });
      }
    });
  }

  async expireStaleLeases(now: Date): Promise<Lease[]> {
    return this.conn.withTx(() => {
      const nowIso = now.toISOString();
      const rows = this.conn.all(`SELECT * FROM leases WHERE expires_at <= $now`, {
        $now: nowIso,
      }) as LeaseRow[];
      if (rows.length > 0) {
        this.conn.run(`DELETE FROM leases WHERE expires_at <= $now`, {
          $now: nowIso,
        });
      }
      return rows.map(leaseFromRow);
    });
  }

  async appendEvents(
    taskId: string,
    events: TaskEventEnvelope[],
    opts: AppendEventsOptions,
  ): Promise<void> {
    if (events.length === 0) return;
    this.conn.withTx(() => {
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
    });
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

  async claimNext(input: ClaimNextInput): Promise<ClaimedTask | null> {
    return this.conn.withAsyncTx(async () => {
      const nowIso = input.now.toISOString();
      const row = this.selectClaimCandidate(
        nowIso,
        input.scopesBusy,
        input.kinds,
      );
      if (!row) return null;
      const task = await this.transitionTask(row.task_id, ["queued"], "running", {
        startedAt: this.clock.nowIso(),
      });
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
    });
  }

  async markDeadLettered(taskId: string, reason: string): Promise<TaskRecord> {
    return this.conn.withTx(() => {
      const row = this.selectTaskRow(taskId);
      if (!row) throw new RecordNotFoundError(`Task not found: ${taskId}`);
      const at = this.clock.nowIso();
      this.conn.run(
        `UPDATE tasks SET dead_lettered_at = $at, dead_letter_reason = $reason WHERE task_id = $id`,
        { $at: at, $reason: reason, $id: taskId },
      );
      return taskFromRow(this.selectTaskRow(taskId)!);
    });
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
   * effective priority (`priority + floor(waitMs / 30s)`) desc, then
   * `enqueued_at` asc, then `rowid` asc as a final deterministic tie-break
   * (insertion order, for when two rows share a millisecond timestamp).
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
  private selectClaimCandidate(
    nowIso: string,
    scopesBusy: string[],
    kinds: string[] | undefined,
  ): TaskRow | null {
    let sql = `SELECT * FROM tasks WHERE status = 'queued' AND available_at <= $now`;
    const params: Params = { $now: nowIso };
    if (scopesBusy.length > 0) {
      const placeholders = scopesBusy.map((_, i) => `$busy${i}`).join(", ");
      sql += ` AND scope_id NOT IN (${placeholders})`;
      scopesBusy.forEach((scope, i) => {
        params[`$busy${i}`] = scope;
      });
    }
    if (kinds !== undefined) {
      if (kinds.length === 0) return null;
      const placeholders = kinds.map((_, i) => `$kind${i}`).join(", ");
      sql += ` AND kind IN (${placeholders})`;
      kinds.forEach((kind, i) => {
        params[`$kind${i}`] = kind;
      });
    }
    sql += ` ORDER BY (priority + CAST((strftime('%s', $now) - strftime('%s', enqueued_at)) / 30 AS INTEGER)) DESC,
             enqueued_at ASC, rowid ASC LIMIT 1`;
    return (this.conn.get(sql, params) as TaskRow | undefined) ?? null;
  }
}

// ---------------------------------------------------------------------------
// ProposalStore
// ---------------------------------------------------------------------------

class SqliteProposalStore implements ProposalStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
  ) {}

  async create(input: CreateProposalInput): Promise<ProposalRecord> {
    return this.conn.withTx(() => {
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
    });
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
    return this.conn.withTx(() => {
      const row = this.selectProposalRow(proposalId);
      if (!row) throw new RecordNotFoundError(`Proposal not found: ${proposalId}`);
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
          $decision: patch?.decision !== undefined ? toJson(patch.decision) : null,
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
    });
  }

  async recordOutcome(
    operationId: string,
    outcome: ApplyOutcome,
  ): Promise<ApplyOutcome> {
    return this.conn.withTx(() => {
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
    });
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
    return this.conn.withTx(() => {
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
    });
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
  constructor(private readonly conn: SqliteConnection) {}

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
          config.extraHeaders === undefined ? null : toJson(config.extraHeaders),
        $metadata: config.metadata === undefined ? null : toJson(config.metadata),
      },
    );
    return config;
  }

  async deleteProvider(providerId: string): Promise<void> {
    this.conn.withTx(() => {
      this.conn.run(`DELETE FROM providers WHERE id = $id`, { $id: providerId });
      this.conn.run(`DELETE FROM provider_models WHERE provider_id = $id`, {
        $id: providerId,
      });
      this.conn.run(`DELETE FROM provider_capabilities WHERE provider_id = $id`, {
        $id: providerId,
      });
    });
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
    this.conn.withTx(() => {
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
    });
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
  }
}

class SqliteSettingsStore implements SettingsStore {
  constructor(private readonly conn: SqliteConnection) {}

  async getSettings(): Promise<AssistantSettings> {
    const row = this.conn.get(`SELECT * FROM settings WHERE id = 1`) as SettingsRow;
    return settingsFromRow(row);
  }

  async updateSettings(
    patch: Partial<AssistantSettings>,
  ): Promise<AssistantSettings> {
    return this.conn.withTx(() => {
      const row = this.conn.get(`SELECT * FROM settings WHERE id = 1`) as SettingsRow;
      const merged: AssistantSettings = { ...settingsFromRow(row), ...patch };
      this.conn.run(
        `UPDATE settings SET
           default_provider_id = $defaultProviderId, default_model = $defaultModel,
           context_size_preference = $contextSizePreference, write_policy_mode = $writePolicyMode,
           allow_raw_tool_data = $allowRawToolData, max_tool_iterations = $maxToolIterations, metadata = $metadata
         WHERE id = 1`,
        {
          $defaultProviderId: merged.defaultProviderId ?? null,
          $defaultModel: merged.defaultModel ?? null,
          $contextSizePreference: merged.contextSizePreference,
          $writePolicyMode: merged.writePolicyMode,
          $allowRawToolData: toIntBool(merged.allowRawToolData),
          $maxToolIterations: merged.maxToolIterations ?? null,
          $metadata: toJson(merged.metadata),
        },
      );
      return merged;
    });
  }
}

class SqliteOutboxStore implements OutboxStore {
  constructor(
    private readonly conn: SqliteConnection,
    private readonly clock: Clock,
    private readonly claimVisibilityMs: number = DEFAULT_OUTBOX_CLAIM_VISIBILITY_MS,
  ) {}

  async enqueue(input: OutboxAppendInput): Promise<OutboxRecord> {
    const now = this.clock.nowIso();
    const id = input.id ?? `outbox_${crypto.randomUUID()}`;
    const availableAt = input.availableAt ?? now;
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
    return this.conn.withTx(() => {
      const nowIso = input.now.toISOString();
      const rows = this.conn.all(
        `SELECT * FROM outbox WHERE published_at IS NULL AND available_at <= $now
         ORDER BY available_at ASC, rowid ASC LIMIT $limit`,
        { $now: nowIso, $limit: input.limit },
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
    });
  }

  async markPublished(id: string, at: Date): Promise<void> {
    this.conn.run(`UPDATE outbox SET published_at = $at WHERE id = $id`, {
      $at: at.toISOString(),
      $id: id,
    });
  }

  async markFailed(id: string, error: string, retryAt: Date): Promise<void> {
    this.conn.run(
      `UPDATE outbox SET last_error = $error, available_at = $retryAt WHERE id = $id`,
      { $error: error, $retryAt: retryAt.toISOString(), $id: id },
    );
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
    // `SCHEMA_V2` against it, which is exactly what falling through would do.
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

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

export interface SqliteAssistantStoreOptions {
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
 * bun:sqlite-backed, complete {@link AssistantStore}.
 *
 * `transaction(fn)` opens a real `BEGIN IMMEDIATE` and commits or rolls back
 * around `fn` — unlike `MemoryAssistantStore`, a throw inside `fn` discards
 * every write `fn` made. Nested `transaction()` calls (including a port
 * method that itself opens a mini-transaction, like `transitionTask` or
 * `createAttempt`) are FLATTENED into the outermost one rather than nested —
 * `bun:sqlite` has no savepoint support in this v1, so re-entrant calls just
 * run against the already-open transaction. See {@link SqliteConnection}.
 */
export class SqliteAssistantStore implements AssistantStore {
  private readonly conn: SqliteConnection;
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
    const db = new Database(path);
    if (path !== ":memory:") {
      db.exec("PRAGMA journal_mode = WAL;");
    }
    db.exec("PRAGMA foreign_keys = ON;");
    assertSchemaVersion(db, path);
    // Idempotent: every statement is CREATE ... IF NOT EXISTS / INSERT OR
    // IGNORE, so reopening the same file (or a file another process already
    // initialized) is a safe no-op.
    db.exec(SCHEMA_V2);
    db.exec(`PRAGMA user_version = ${SCHEMA_VERSION};`);
    this.conn = new SqliteConnection(db);
    const clock = options.clock ?? defaultClock;
    const ids = options.ids ?? defaultIds;
    this.conversations = new SqliteConversationStore(this.conn, clock, ids);
    this.tasks = new SqliteTaskStore(this.conn, clock, ids, options.leaseTtlMs);
    this.proposals = new SqliteProposalStore(this.conn, clock);
    this.providers = new SqliteProviderStore(this.conn);
    this.settings = new SqliteSettingsStore(this.conn);
    this.outbox = new SqliteOutboxStore(
      this.conn,
      clock,
      options.outboxClaimVisibilityMs,
    );
  }

  async transaction<T>(fn: (tx: AssistantStore) => Promise<T>): Promise<T> {
    return this.conn.withAsyncTx(() => fn(this));
  }

  /** Closes the underlying connection. Safe to call once; further use throws. */
  close(): void {
    this.conn.db.close();
  }
}
