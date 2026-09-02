/**
 * Row shapes and row<->record mapping for every table this adapter owns, plus
 * the small JSON/bool/instant helpers every mapper and store shares.
 *
 * Split out of `sqlite-assistant-store.ts` so a sub-store module can map its
 * own rows without importing the whole aggregate — see
 * {@link ../sqlite-assistant-store.js}.
 */
import type {
  AiContentPart,
  AiMessageContent,
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
  AiToolCall,
} from "@agentkit/contracts";
import {
  AgentKitHostError,
  type ApplyOutcome,
  type AssistantSettings,
  type AttemptRecord,
  type AttemptStatus,
  type ChatRecord,
  type Lease,
  type MessageRecord,
  type OutboxRecord,
  type ProposalDecision,
  type ProposalRecord,
  type ProposalStatus,
  type RiskLevel,
  type TaskRecord,
  type TaskStatus,
  type ToolCallingMode,
  type WritePolicyMode,
} from "@agentkit/host";

export function toJson(value: unknown): string {
  return JSON.stringify(value ?? null);
}
/** For NOT NULL columns that always hold valid JSON (written with a default). */
export function parseJson<T>(text: string): T {
  return JSON.parse(text) as T;
}
/** For nullable columns where NULL means "the field was absent". */
export function parseNullableJson<T>(text: string | null): T | undefined {
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
export function normalizeInstant(value: string, field: string): string {
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
export function encodeContent(content: AiMessageContent): EncodedContent {
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
export function decodeContent(
  content: string,
  format: string,
): AiMessageContent {
  return format === "parts"
    ? (JSON.parse(content) as AiContentPart[])
    : content;
}

export function toIntBool(value: boolean): number {
  return value ? 1 : 0;
}
export function fromIntBool(value: number): boolean {
  return value === 1;
}
export function toOptionalIntBool(value: boolean | undefined): number | null {
  return value === undefined ? null : value ? 1 : 0;
}
export function fromOptionalIntBool(value: number | null): boolean | undefined {
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
export function isConstraintError(err: unknown): boolean {
  const code = (err as { code?: string } | null)?.code;
  return (
    code === "SQLITE_CONSTRAINT_UNIQUE" ||
    code === "SQLITE_CONSTRAINT_PRIMARYKEY"
  );
}
export interface ChatRow {
  id: string;
  title: string | null;
  created_at: string;
  updated_at: string;
  metadata: string;
  archived: number;
}
export interface MessageRow {
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
export interface TaskRow {
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
export interface AttemptRow {
  attempt_id: string;
  task_id: string;
  attempt_number: number;
  status: string;
  owner_id: string;
  started_at: string;
  ended_at: string | null;
  error: string | null;
}
export interface LeaseRow {
  task_id: string;
  attempt_id: string;
  owner_id: string;
  lease_token: string;
  fencing_token: number;
  expires_at: string;
}
export interface TaskEventRow {
  task_id: string;
  seq: number;
  event_id: string;
  attempt_id: string | null;
  type: string;
  timestamp: string;
  payload: string;
}
export interface OutboxRow {
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
export interface ProposalRow {
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
  claimed_at: string | null;
  applied_at: string | null;
}
export interface ProposalOutcomeRow {
  operation_id: string;
  status: string;
  applied_ops: number;
  failed_ops: string;
  result_json: string | null;
  revision: string | null;
}
export interface ProviderRow {
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
export interface ProviderModelRow {
  provider_id: string;
  model_id: string;
  display_name: string | null;
  context_window_tokens: number | null;
  supports_tool_calling: number | null;
  fetched_at: string;
}
export interface ProviderCapabilitiesRow {
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
export interface SettingsRow {
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

export function chatFromRow(row: ChatRow): ChatRecord {
  return {
    id: row.id,
    ...(row.title === null ? {} : { title: row.title }),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
    metadata: parseJson<Record<string, unknown>>(row.metadata),
    archived: fromIntBool(row.archived),
  };
}

export function messageFromRow(row: MessageRow): MessageRecord {
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

export function taskFromRow(row: TaskRow): TaskRecord {
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

export function attemptFromRow(row: AttemptRow): AttemptRecord {
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

export function leaseFromRow(row: LeaseRow): Lease {
  return {
    taskId: row.task_id,
    attemptId: row.attempt_id,
    ownerId: row.owner_id,
    leaseToken: row.lease_token,
    fencingToken: row.fencing_token,
    expiresAt: row.expires_at,
  };
}

export function proposalFromRow(row: ProposalRow): ProposalRecord {
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
    ...(row.claimed_at === null ? {} : { claimedAt: row.claimed_at }),
    ...(row.applied_at === null ? {} : { appliedAt: row.applied_at }),
  };
}

export function outcomeFromRow(row: ProposalOutcomeRow): ApplyOutcome {
  return {
    status: row.status as ApplyOutcome["status"],
    appliedOps: row.applied_ops,
    failedOps: parseJson<{ opIndex: number; error: string }[]>(row.failed_ops),
    ...(row.result_json === null ? {} : { resultJson: row.result_json }),
    ...(row.revision === null ? {} : { revision: row.revision }),
  };
}

export function providerFromRow(row: ProviderRow): AiProviderConfig {
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

export function modelFromRow(row: ProviderModelRow): AiProviderModel {
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

export function capabilitiesFromRow(
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

export function settingsFromRow(row: SettingsRow): AssistantSettings {
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

export function outboxFromRow(row: OutboxRow): OutboxRecord {
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
