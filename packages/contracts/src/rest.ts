/**
 * The versioned HTTP surface — types and JSON Schemas only.
 *
 * There is no server here, and no client: this module is the agreement an HTTP
 * adapter would serialize to, written down first so the shapes are reviewable,
 * validatable, and versioned independently of whoever eventually serves them
 * (see [`docs/non-goals.md`](../../../docs/non-goals.md) — the adapter itself
 * remains deferred).
 *
 * **DTOs are projections, not records.** Each DTO below mirrors a host record
 * (`packages/host/src/ports/*.ts`) with its internals removed: leases and
 * fencing tokens, queue bookkeeping, the host-shaped `envelope`/`operations`
 * bodies of a proposal, idempotency keys. Those exist so the orchestrator can
 * recover from a crash; publishing them would freeze private mechanics into a
 * public contract, and none of them mean anything to a client. Every omission is
 * called out on the DTO that makes it.
 *
 * **Some enumerations are mirrored, not imported.** `RunStatusDto`,
 * `ProposalStatusDto`, `RiskLevelDto`, `WritePolicyModeDto` and
 * `ToolCallingModeDto` restate unions that `@agentkit/host` owns, and
 * `McpTransportDto` restates one `@agentkit/mcp-client` owns, because contracts
 * sits *below* both and cannot depend on either. They must be kept in step by
 * hand; the compile-time cross-check for the host ones lives in
 * `packages/host/tests/state-machines.test.ts`, and each one names its source
 * below.
 */
import { Type, type Static } from "@sinclair/typebox";
import { AiContentPartSchema } from "./content.js";
import {
  AiProviderKindSchema,
  AiProviderModelSchema,
  type AiProviderModel,
} from "./provider.js";
import { AiRunEventSchema, type AiRunEvent } from "./run-events.js";
import { AiSourceRefSchema } from "./source-ref.js";
import {
  AiContextSizePreferenceSchema,
  AiToolCallSchema,
  AiToolDefinitionSchema,
  AiToolStatusSchema,
  type AiToolDefinition,
} from "./tool.js";

// ---------------------------------------------------------------------------
// Version + route table
// ---------------------------------------------------------------------------

/**
 * The URL-visible API version, bumped only when a route or DTO changes
 * incompatibly. Distinct from `CONTRACT_VERSION` (the event/DTO *shape*
 * version): an additive DTO field bumps that and leaves this alone, because a
 * client written against `/v1` keeps working.
 */
export const REST_API_VERSION = "v1";

export type RestMethod = "GET" | "POST" | "PATCH" | "DELETE";

export interface RouteDef {
  method: RestMethod;
  /** Path with `:param` placeholders, rooted at the API version prefix. */
  path: string;
}

/**
 * The whole HTTP surface, as data — so an adapter, a client generator, and a
 * routing table cannot drift apart by transcription. Keyed by operation name.
 *
 * `as const satisfies Readonly<Record<string, RouteDef>>`: the `satisfies` half
 * type-checks every entry against {@link RouteDef} without widening the value,
 * and the `as const` half keeps the literals, so a consumer reads
 * `REST_ROUTES.getRun.path` as `"/v1/runs/:runId"` rather than as `string`.
 *
 * Two routes carry header semantics no path can express:
 *
 * - **`submitMessage`** and **`regenerateMessage`** REQUIRE an
 *   `Idempotency-Key` header. Each creates a run and at least one message; a
 *   client that retries a timed-out POST without a key duplicates the turn, and
 *   no amount of server-side cleverness can tell that retry apart from a user
 *   who really did send twice. They are the only two routes that require it.
 * - **`streamRun`** is Server-Sent Events, and resumes on `Last-Event-ID`: the
 *   value is an `AiRunEvent.eventId`, and the server replays everything after
 *   that event from the run's durable log. This is why `eventId` and `seq` are
 *   required base fields rather than decoration.
 */
export const REST_ROUTES = {
  createChat: { method: "POST", path: "/v1/chats" },
  listChats: { method: "GET", path: "/v1/chats" },
  getChat: { method: "GET", path: "/v1/chats/:chatId" },
  /** Rename, re-tag or (un)archive. See {@link UpdateChatRequestSchema}. */
  updateChat: { method: "PATCH", path: "/v1/chats/:chatId" },
  /**
   * The chat, its messages (every branch), its runs and its proposals — 204 on
   * success, `chat_busy` (409) while a run in the chat is still live.
   */
  deleteChat: { method: "DELETE", path: "/v1/chats/:chatId" },

  listMessages: { method: "GET", path: "/v1/chats/:chatId/messages" },
  /** Requires an `Idempotency-Key` header; see the note above. */
  submitMessage: { method: "POST", path: "/v1/chats/:chatId/messages" },
  /**
   * Answer the same question again, as a new branch beside the answer it
   * already has. Requires an `Idempotency-Key` header, like `submitMessage`,
   * and answers with the same {@link SubmitMessageResponseSchema}.
   */
  regenerateMessage: {
    method: "POST",
    path: "/v1/chats/:chatId/messages/:messageId/regenerate",
  },
  /** Copies the active path up to a message into a NEW chat. See {@link ForkChatRequestSchema}. */
  forkChat: { method: "POST", path: "/v1/chats/:chatId/fork" },

  /**
   * Full-text search over message bodies: `?q=` (required), `?chatId=` to scope
   * to one conversation, `?limit=`.
   *
   * Rooted at `/v1/search` rather than nested under a chat because the
   * unscoped search — "where did I see that?" across every conversation — is
   * the one this route mainly exists for, and a chat-nested path could not
   * express it.
   */
  searchMessages: { method: "GET", path: "/v1/search" },

  /**
   * Make a message's branch the active one, and answer with the path that
   * became active. Rooted at `/v1/messages/:messageId` rather than nested under
   * the chat because a message id already names exactly one chat, and a route
   * that carried both would let a client pass a mismatched pair for the server
   * to arbitrate.
   */
  activateBranch: { method: "POST", path: "/v1/messages/:messageId/activate" },
  /** The message's siblings (same parent, INCLUDING itself), `branchIndex` ascending. */
  listSiblings: { method: "GET", path: "/v1/messages/:messageId/siblings" },

  getRun: { method: "GET", path: "/v1/runs/:runId" },
  /** SSE. Resumes from `Last-Event-ID` (an `AiRunEvent.eventId`). */
  streamRun: { method: "GET", path: "/v1/runs/:runId/stream" },
  cancelRun: { method: "POST", path: "/v1/runs/:runId/cancel" },

  listToolEvents: { method: "GET", path: "/v1/chats/:chatId/tool-events" },

  listProposals: { method: "GET", path: "/v1/chats/:chatId/proposals" },
  approveProposal: {
    method: "POST",
    path: "/v1/proposals/:proposalId/approve",
  },
  rejectProposal: { method: "POST", path: "/v1/proposals/:proposalId/reject" },
  applyProposal: { method: "POST", path: "/v1/proposals/:proposalId/apply" },

  listProviders: { method: "GET", path: "/v1/providers" },
  createProvider: { method: "POST", path: "/v1/providers" },
  updateProvider: { method: "PATCH", path: "/v1/providers/:providerId" },
  deleteProvider: { method: "DELETE", path: "/v1/providers/:providerId" },
  listModels: { method: "GET", path: "/v1/providers/:providerId/models" },
  /**
   * Re-probe the provider's catalogue and replace it. A WRITE, not a cached
   * read: it costs a request to someone else's server, so it is a POST a client
   * asks for rather than something `listModels` does on a stale timestamp.
   */
  refreshProviderModels: {
    method: "POST",
    path: "/v1/providers/:providerId/models/refresh",
  },
  /** Probe the endpoint's reachability and credentials. See {@link TestProviderResponseSchema}. */
  testProvider: { method: "POST", path: "/v1/providers/:providerId/test" },

  getSettings: { method: "GET", path: "/v1/settings" },
  updateSettings: { method: "PATCH", path: "/v1/settings" },

  /** Standing write grants for one chat: `?chatId=` is REQUIRED. */
  listAllowances: { method: "GET", path: "/v1/write-policy/allowances" },
  grantAllowance: { method: "POST", path: "/v1/write-policy/allowances" },
  /** `?chatId=` is REQUIRED — a grant is revoked by the chat that owns it. */
  revokeAllowance: {
    method: "DELETE",
    path: "/v1/write-policy/allowances/:allowanceId",
  },

  listMcpServers: { method: "GET", path: "/v1/mcp/servers" },
  createMcpServer: { method: "POST", path: "/v1/mcp/servers" },
  updateMcpServer: { method: "PATCH", path: "/v1/mcp/servers/:serverId" },
  deleteMcpServer: { method: "DELETE", path: "/v1/mcp/servers/:serverId" },

  listTools: { method: "GET", path: "/v1/tools" },
  getVersion: { method: "GET", path: "/v1/version" },
} as const satisfies Readonly<Record<string, RouteDef>>;

/** Operation names in {@link REST_ROUTES}. */
export type RestOperation = keyof typeof REST_ROUTES;

// ---------------------------------------------------------------------------
// Mirrored host enumerations
// ---------------------------------------------------------------------------

/**
 * MIRROR of `TaskStatus` (`packages/host/src/ports/task-store.ts`). Note that
 * `waiting_approval` is currently producer-less in this repository — a staged
 * write returns pending to the model and the run completes — but it is part of
 * the vocabulary a client must handle, since a host that parks runs on approval
 * uses the same wire contract.
 */
export const RunStatusDtoSchema = Type.Union([
  Type.Literal("queued"),
  Type.Literal("running"),
  Type.Literal("waiting_approval"),
  Type.Literal("completed"),
  Type.Literal("failed"),
  Type.Literal("cancelled"),
]);
export type RunStatusDto = Static<typeof RunStatusDtoSchema>;

/** MIRROR of `ProposalStatus` (`packages/host/src/ports/proposal-store.ts`). */
export const ProposalStatusDtoSchema = Type.Union([
  Type.Literal("pending"),
  Type.Literal("approved"),
  Type.Literal("applying"),
  Type.Literal("applied"),
  Type.Literal("failed"),
  Type.Literal("rejected"),
  Type.Literal("invalidated"),
]);
export type ProposalStatusDto = Static<typeof ProposalStatusDtoSchema>;

/**
 * MIRROR of `RiskLevel` (`packages/host/src/ports/proposal-store.ts`). Ordered
 * low → destructive; a write policy's allowance at rank N covers everything at
 * rank ≤ N.
 */
export const RiskLevelDtoSchema = Type.Union([
  Type.Literal("low"),
  Type.Literal("medium"),
  Type.Literal("high"),
  Type.Literal("destructive"),
]);
export type RiskLevelDto = Static<typeof RiskLevelDtoSchema>;

/** MIRROR of `WritePolicyMode` (`packages/host/src/ports/write-policy.ts`). */
export const WritePolicyModeDtoSchema = Type.Union([
  Type.Literal("auto_readonly_confirm_writes"),
  Type.Literal("confirm_all_writes"),
  Type.Literal("auto_all"),
]);
export type WritePolicyModeDto = Static<typeof WritePolicyModeDtoSchema>;

/** MIRROR of `ToolCallingMode` (`packages/host/src/ports/settings-store.ts`). */
export const ToolCallingModeDtoSchema = Type.Union([
  Type.Literal("auto"),
  Type.Literal("on"),
  Type.Literal("off"),
]);
export type ToolCallingModeDto = Static<typeof ToolCallingModeDtoSchema>;

// ---------------------------------------------------------------------------
// Resource DTOs
// ---------------------------------------------------------------------------

/** A conversation. Projection of `ChatRecord`. */
export const ChatDtoSchema = Type.Object({
  id: Type.String(),
  title: Type.Optional(Type.String()),
  createdAt: Type.String({ description: "ISO-8601." }),
  updatedAt: Type.String({ description: "ISO-8601." }),
  /**
   * Hidden from the default `listChats` listing, and otherwise an ordinary
   * chat: it still answers `getChat`, still accepts turns.
   *
   * REQUIRED rather than optional, unlike the branching fields on
   * `MessageDto`: every chat has an archived state, a server that models no
   * archiving sends `false`, and a client rendering a tri-state checkbox off an
   * absent boolean is a bug waiting for the first pre-archiving server.
   */
  archived: Type.Boolean(),
  metadata: Type.Record(Type.String(), Type.Unknown()),
});
export type ChatDto = Static<typeof ChatDtoSchema>;

/**
 * One persisted message. Projection of `MessageRecord`.
 *
 * OMITTED, deliberately: `orderKey` (the store's internal ordering key — the
 * array order carries it, and {@link MessagePageDto.nextCursor} is the opaque
 * handle a client pages with) and `modelResultJson` (the slim envelope the host
 * replays to the provider; for a `role: "tool"` record it is already what
 * `content` holds, so shipping both would invite a client to pick the wrong one).
 *
 * `metadata` is where the host's reserved flags surface — `internal: true` marks
 * a replay-only record (the assistant turn carrying `toolCalls`, and the tool
 * results answering it), `placeholder: true` an answer still streaming.
 *
 * The three branching fields are OPTIONAL for one reason only: a server written
 * against a pre-branching contract (or a degenerate store that never modelled a
 * tree) omits them, and a client must render that conversation as the straight
 * line it is rather than refuse it. A server that does model the tree always
 * sends all three. `depth` is deliberately NOT published — it is derivable from
 * the parent chain a client already has, and a second source of truth for
 * position is a second thing that can disagree.
 */
export const MessageDtoSchema = Type.Object({
  id: Type.String(),
  chatId: Type.String(),
  runId: Type.Optional(Type.String()),
  role: Type.Union([
    Type.Literal("user"),
    Type.Literal("assistant"),
    Type.Literal("tool"),
    Type.Literal("system"),
  ]),
  /**
   * The message body: a plain string, or the ordered content parts of a
   * multimodal message. The projection passes whatever the store holds through
   * UNCHANGED — including an image part whose `source.kind` is `"ref"`, the
   * host-side attachment handle described on `AiImageSourceSchema`.
   *
   * Publishing the ref rather than resolving it here is deliberate. Resolving
   * would inline megabytes of base64 into every page of every conversation, for
   * a client that usually wants a thumbnail from its own endpoint and sometimes
   * wants nothing at all. A client that understands the host's refs renders
   * them; one that does not ignores the part, exactly as it would ignore a part
   * type from a later contract version.
   */
  content: Type.Union([Type.String(), Type.Array(AiContentPartSchema)]),
  toolCalls: Type.Optional(Type.Array(AiToolCallSchema)),
  toolCallId: Type.Optional(Type.String()),
  /** The message this one answers. Absent on a root — and on a pre-branching server. */
  parentMessageId: Type.Optional(Type.String()),
  /**
   * Position among siblings sharing this parent, ascending. `0` is the first
   * answer written; each later regeneration of the same question gets the next
   * index, so the number is stable for the life of the message.
   */
  branchIndex: Type.Optional(Type.Number()),
  /** Whether this message is on the chat's currently active path. */
  active: Type.Optional(Type.Boolean()),
  metadata: Type.Record(Type.String(), Type.Unknown()),
  createdAt: Type.String({ description: "ISO-8601." }),
});
export type MessageDto = Static<typeof MessageDtoSchema>;

/**
 * One page of messages, oldest first. `nextCursor` is opaque: it encodes the
 * store's ordering key, and a client that tried to interpret it would be reading
 * an implementation detail this contract does not promise.
 *
 * The page covers the chat's ACTIVE PATH — the branch currently selected, root
 * to leaf — not every message ever written to the chat. A conversation nobody
 * has branched has exactly one path, so this is the same page it always was;
 * a branched one answers with the branch a reader is looking at, which is the
 * only page that reads as a conversation. Off-path siblings are reachable
 * through `listSiblings`, and `activateBranch` is how a client changes which
 * path this route reports.
 */
export const MessagePageDtoSchema = Type.Object({
  items: Type.Array(MessageDtoSchema),
  nextCursor: Type.Optional(Type.String()),
});
export type MessagePageDto = Static<typeof MessagePageDtoSchema>;

/**
 * A durable run. Projection of `TaskRecord`.
 *
 * `createdAt` is the record's `enqueuedAt` — when the turn was accepted, which
 * is what a client renders; the name is the client-facing one because "enqueued"
 * describes a queue the API does not expose.
 *
 * OMITTED, deliberately: `priority`, `availableAt`, `attemptCount`,
 * `poisonCount`, `deadLetteredAt`/`deadLetterReason` (queue bookkeeping — a
 * client cannot act on any of it, and a run that exhausted its attempts is
 * simply `failed` with an `error`), and `payload` (the internal turn envelope:
 * message ids and provider selection the server assembled for itself).
 */
export const RunDtoSchema = Type.Object({
  runId: Type.String(),
  chatId: Type.String(),
  /**
   * What this run writes to — the serialization key. Usually the chat; a host
   * writing a shared document scopes on the document, so two chats editing it
   * are serialized against each other.
   */
  scopeId: Type.String(),
  status: RunStatusDtoSchema,
  createdAt: Type.String({ description: "ISO-8601; the record's enqueuedAt." }),
  startedAt: Type.Optional(Type.String()),
  finishedAt: Type.Optional(Type.String()),
  error: Type.Optional(Type.String()),
});
export type RunDto = Static<typeof RunDtoSchema>;

/** Who decided a proposal, and on what authority. Projection of `ProposalDecision`. */
export const ProposalDecisionDtoSchema = Type.Object({
  /**
   * `"policy"` is a machine approval under a standing allowance and always
   * carries `policyId`; `"user"` never does. The audit trail has to be able to
   * tell an auto-applied write from a human-reviewed one.
   */
  actor: Type.Union([Type.Literal("user"), Type.Literal("policy")]),
  decidedBy: Type.Optional(Type.String()),
  policyId: Type.Optional(Type.String()),
  reason: Type.Optional(Type.String()),
  decidedAt: Type.String({ description: "ISO-8601." }),
});
export type ProposalDecisionDto = Static<typeof ProposalDecisionDtoSchema>;

/**
 * What one apply attempt did. Projection of `ApplyOutcome`.
 *
 * `partial` is a real outcome, not a rounding of either neighbour: some
 * operations landed and some did not, and the proposal's own status is still
 * `applied` (the write happened) — the partiality lives here.
 *
 * OMITTED: `resultJson` (host payload describing what it created) and
 * `revision` (the scope revision after the apply, used for staleness checks
 * server-side).
 */
export const ApplyOutcomeDtoSchema = Type.Object({
  status: Type.Union([
    Type.Literal("applied"),
    Type.Literal("partial"),
    Type.Literal("failed"),
  ]),
  appliedOps: Type.Number(),
  failedOps: Type.Array(
    Type.Object({ opIndex: Type.Number(), error: Type.String() }),
  ),
});
export type ApplyOutcomeDto = Static<typeof ApplyOutcomeDtoSchema>;

/**
 * A staged write. Projection of `ProposalRecord`.
 *
 * OMITTED, deliberately: `envelope` and `operations` (the host-shaped body of
 * the write — arbitrary domain JSON this contract cannot type, and what a
 * reviewer needs rendered is the host's business, through the host's own UI),
 * `revisionAtCreate` (the staleness check's input) and `operationId` (the apply
 * idempotency key the server mints and matches).
 *
 * `summary` has no column behind it: it is the one line a host projects for a
 * reviewer, typically from its own envelope, and for a terminal proposal the
 * record's `reason` (`revision_conflict`, `interrupted`, an applier's error) is
 * the natural source. It is optional precisely because a host that has nothing
 * short to say should say nothing.
 */
export const ProposalDtoSchema = Type.Object({
  id: Type.String(),
  chatId: Type.String(),
  runId: Type.Optional(Type.String()),
  /** The write's namespace: idempotency and serialization both key on it. */
  scopeKey: Type.String(),
  /** Model-supplied idempotency key; unique per scope while it is held. */
  actionId: Type.Optional(Type.String()),
  toolName: Type.String(),
  /** Host-defined proposal kind (the write's shape), e.g. `"document.edits"`. */
  kind: Type.String(),
  risk: RiskLevelDtoSchema,
  status: ProposalStatusDtoSchema,
  summary: Type.Optional(Type.String()),
  warnings: Type.Array(Type.String()),
  /** The builder dropped operations to fit a cap; never auto-applied. */
  truncated: Type.Boolean(),
  decision: Type.Optional(ProposalDecisionDtoSchema),
  outcome: Type.Optional(ApplyOutcomeDtoSchema),
  createdAt: Type.String({ description: "ISO-8601." }),
  decidedAt: Type.Optional(Type.String()),
  appliedAt: Type.Optional(Type.String()),
});
export type ProposalDto = Static<typeof ProposalDtoSchema>;

/**
 * One tool call's life, as a client renders it.
 *
 * There is no tool-event record in the host layer: this is a projection of the
 * `run.tool.*` family in the run event log (`id` is the `eventId` of the event
 * that produced this state, `status` its stage). `listToolEvents` exists so a
 * client can render a chat's tool history without replaying every run's whole
 * stream.
 *
 * The slim/full split survives the projection: `resultJson` is the full payload
 * (for the UI), `modelResultJson` the slim envelope the model was actually fed.
 */
export const ToolEventDtoSchema = Type.Object({
  id: Type.String({ description: "The eventId of the originating run event." }),
  runId: Type.String(),
  chatId: Type.String(),
  toolCallId: Type.String(),
  toolName: Type.String(),
  status: AiToolStatusSchema,
  argumentsJson: Type.Optional(Type.String()),
  resultJson: Type.Optional(Type.String()),
  modelResultJson: Type.Optional(Type.String()),
  summary: Type.Optional(Type.String()),
  errorCode: Type.Optional(Type.String()),
  errorMessage: Type.Optional(Type.String()),
  sources: Type.Optional(Type.Array(AiSourceRefSchema)),
  warnings: Type.Optional(Type.Array(Type.String())),
  truncated: Type.Optional(Type.Boolean()),
  createdAt: Type.String({ description: "ISO-8601." }),
});
export type ToolEventDto = Static<typeof ToolEventDtoSchema>;

/**
 * A tool as advertised to a client. Identical to {@link AiToolDefinitionSchema}
 * — the definition a model is shown and the definition a client lists are the
 * same document, and forking them would let a UI describe a tool the model never
 * saw.
 */
export const ToolDefinitionDtoSchema = AiToolDefinitionSchema;
export type ToolDefinitionDto = AiToolDefinition;

/**
 * One message matching `searchMessages`. Projection of `MessageSearchHit`.
 *
 * `snippet` is a window around the match with the matched terms wrapped in
 * `[`/`]` and elided text standing in as `…` — the markers are fixed by the
 * host port so a client can strip or style them without asking which store is
 * underneath. Deliberately not HTML: a store does not know what its caller
 * renders into.
 *
 * The message BODY is not here, and neither is its role or its chat's title. A
 * hit is a pointer plus its evidence; a client that wants the message reads the
 * conversation it names.
 */
export const MessageSearchHitDtoSchema = Type.Object({
  chatId: Type.String(),
  messageId: Type.String(),
  snippet: Type.String(),
});
export type MessageSearchHitDto = Static<typeof MessageSearchHitDtoSchema>;

/** What `searchMessages` answers: best match first. */
export const MessageSearchResponseSchema = Type.Object({
  hits: Type.Array(MessageSearchHitDtoSchema),
});
export type MessageSearchResponse = Static<typeof MessageSearchResponseSchema>;

/**
 * A configured provider. Projection of `AiProviderConfig`, and a NARROWING one.
 *
 * OMITTED, deliberately and permanently: `apiKey` (the credential itself),
 * `extraHeaders` (which routinely carries another one), and `metadata` (an
 * open host-owned bag). None of the three tells a client anything it can act
 * on, and all three are the kind of field that leaks once and is in a log
 * forever.
 *
 * `apiKeySecretRef` is the one thing lifted OUT of that metadata bag, because
 * a UI has a real question the omissions make unanswerable: is a key
 * configured at all? A ref is a name, not a secret — it is what a host looks
 * the value up by in its own `SecretStore`, and knowing it grants nothing.
 */
export const ProviderDtoSchema = Type.Object({
  id: Type.String(),
  label: Type.String(),
  kind: AiProviderKindSchema,
  baseUrl: Type.String(),
  defaultModel: Type.String(),
  enabled: Type.Boolean(),
  /** Present when a credential is stored for this provider. NEVER the key. */
  apiKeySecretRef: Type.Optional(Type.String()),
});
export type ProviderDto = Static<typeof ProviderDtoSchema>;

/**
 * One model in a provider's catalogue. Identical to `AiProviderModel` — the
 * record a host stores and the row a client lists are the same document, and
 * forking them would let a UI describe a model the runner cannot select.
 */
export const ModelDtoSchema = AiProviderModelSchema;
export type ModelDto = AiProviderModel;

/**
 * Assistant-wide settings. Projection of `AssistantSettings` — one row, the
 * knobs a settings pane owns, not per-chat state.
 *
 * Nothing is omitted: every field here is a preference a user set and can see.
 * `writePolicyMode` and `toolCalling` are the two mirrored unions
 * ({@link WritePolicyModeDtoSchema}, {@link ToolCallingModeDtoSchema}).
 */
export const SettingsDtoSchema = Type.Object({
  defaultProviderId: Type.Optional(Type.String()),
  defaultModel: Type.Optional(Type.String()),
  contextSizePreference: AiContextSizePreferenceSchema,
  writePolicyMode: WritePolicyModeDtoSchema,
  allowRawToolData: Type.Boolean(),
  maxToolIterations: Type.Optional(Type.Number()),
  toolCalling: Type.Optional(ToolCallingModeDtoSchema),
  metadata: Type.Record(Type.String(), Type.Unknown()),
});
export type SettingsDto = Static<typeof SettingsDtoSchema>;

/**
 * A standing "yes" for one `(chat, tool, proposal kind)`, up to a risk ceiling.
 * Projection of `WriteAllowance`.
 *
 * `key` is the id: `revokeAllowance` takes it in the path, and it is stable for
 * the life of the grant. The ceiling is what keeps the grant honest — an
 * allowance at rank N covers everything at rank ≤ N and nothing above it, so a
 * model cannot escalate by re-labelling its own proposal.
 */
export const WriteAllowanceDtoSchema = Type.Object({
  key: Type.String(),
  chatId: Type.String(),
  toolName: Type.String(),
  proposalKind: Type.String(),
  maxRisk: RiskLevelDtoSchema,
  createdAt: Type.String({ description: "ISO-8601." }),
});
export type WriteAllowanceDto = Static<typeof WriteAllowanceDtoSchema>;

/** What `listAllowances` answers, for one chat. */
export const WriteAllowanceListResponseSchema = Type.Object({
  allowances: Type.Array(WriteAllowanceDtoSchema),
});
export type WriteAllowanceListResponse = Static<
  typeof WriteAllowanceListResponseSchema
>;

/**
 * How to reach one MCP server. MIRROR of `McpTransportConfig`
 * (`packages/mcp-client/src/config.ts`) — contracts sits below that package and
 * cannot import from it, so the union is restated and kept in step by hand,
 * exactly as `RunStatusDto` restates `TaskStatus`.
 *
 * A CLOSED union: an unknown `kind` is rejected rather than stored, because a
 * transport nothing can connect is a record that fails at the worst possible
 * moment — the first run that stages its tools.
 *
 * `env` and `headers` values may contain `${placeholder}` tokens that
 * `secretRefs` resolves at connect time. The tokens are what travel; the values
 * behind them never do.
 */
export const McpStdioTransportDtoSchema = Type.Object({
  kind: Type.Literal("stdio"),
  command: Type.String(),
  args: Type.Optional(Type.Array(Type.String())),
  env: Type.Optional(Type.Record(Type.String(), Type.String())),
});
export const McpHttpTransportDtoSchema = Type.Object({
  kind: Type.Literal("http"),
  url: Type.String(),
  headers: Type.Optional(Type.Record(Type.String(), Type.String())),
});
export const McpTransportDtoSchema = Type.Union([
  McpStdioTransportDtoSchema,
  McpHttpTransportDtoSchema,
]);
export type McpTransportDto = Static<typeof McpTransportDtoSchema>;

/**
 * One configured MCP server. Projection of `McpServerConfigRecord`.
 *
 * NO SECRET MATERIAL, by construction rather than by omission: the record
 * itself carries none. `secretRefs` maps a `${placeholder}` token to a
 * `SecretStore` REF, and the value behind the ref is injected into the env or
 * header at connect time and stored nowhere. Publishing the map is what lets a
 * UI show which credentials a server expects without ever holding one.
 *
 * `alias` is the tool NAMESPACE — it is baked into every canonical tool id
 * (`mcp.<alias>.<tool>`) a transcript records — which is why it is unique and
 * why `id` exists separately: a rename must not break the handle a URL uses.
 *
 * `resilience` is an open record rather than a typed one for the same reason
 * the transport union is mirrored: `McpResilienceOptions` belongs to
 * `@agentkit/mcp-client`, a package this one sits below, and its knobs are
 * tuning rather than contract.
 */
export const McpServerDtoSchema = Type.Object({
  id: Type.String(),
  alias: Type.String({ description: "Tool namespace; `^[a-z][a-z0-9-]*$`." }),
  transport: McpTransportDtoSchema,
  /** `${placeholder}` token → SecretStore ref. Refs only, never values. */
  secretRefs: Type.Optional(Type.Record(Type.String(), Type.String())),
  /** Absent means the default, `true`. A disabled server contributes nothing. */
  enabled: Type.Optional(Type.Boolean()),
  /** Server tool name → the name the canonical id should use instead. */
  toolAliases: Type.Optional(Type.Record(Type.String(), Type.String())),
  resilience: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
  createdAt: Type.String({ description: "ISO-8601." }),
  updatedAt: Type.String({ description: "ISO-8601." }),
});
export type McpServerDto = Static<typeof McpServerDtoSchema>;

// ---------------------------------------------------------------------------
// Request / response bodies
// ---------------------------------------------------------------------------

/** Body of `createChat`. The server mints the id. */
export const CreateChatRequestSchema = Type.Object({
  title: Type.Optional(Type.String()),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type CreateChatRequest = Static<typeof CreateChatRequestSchema>;

/**
 * Body of `submitMessage`. Carry an `Idempotency-Key` header with it: this is
 * the one write in the API that creates three records at once.
 */
export const SubmitMessageRequestSchema = Type.Object({
  /**
   * The turn's body: a plain string, or content parts for a multimodal turn.
   *
   * An image part may name a host attachment (`source.kind: "ref"`) instead of
   * carrying its bytes — that is the shape a client that already uploaded a file
   * sends, and it is what keeps a submit small no matter how large the
   * attachment is. The host resolves refs per provider pass; see
   * `AttachmentResolver` in `@agentkit/host`.
   */
  content: Type.Union([Type.String(), Type.Array(AiContentPartSchema)]),
  /** Overrides the provider's default model for this turn only. */
  model: Type.Optional(Type.String()),
  /**
   * Submit this turn as a NEW BRANCH under the named message instead of at the
   * end of the conversation — the edit-and-regenerate flow, where a user rewrites
   * an earlier question and wants a different answer without losing the first one.
   *
   * The named message must be in this chat. The new turn is created active, and
   * the whole active path switches to it in the same write, so the very next
   * `listMessages` reports the new branch and the run replays THAT history to
   * the provider. Omit it and the turn appends to the active leaf, which is what
   * every non-branching client does and what a linear conversation has always
   * done.
   */
  parentMessageId: Type.Optional(Type.String()),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type SubmitMessageRequest = Static<typeof SubmitMessageRequestSchema>;

/**
 * What `submitMessage` returns — immediately, before a single token exists.
 * The `assistantMessageId` is the placeholder the answer streams into, so a
 * client can render the turn and subscribe to `streamRun` in the same tick.
 */
export const SubmitMessageResponseSchema = Type.Object({
  chatId: Type.String(),
  runId: Type.String(),
  userMessageId: Type.String(),
  assistantMessageId: Type.String(),
});
export type SubmitMessageResponse = Static<typeof SubmitMessageResponseSchema>;

/**
 * Body of `forkChat`. The response is a {@link ChatDtoSchema} — the new chat —
 * and its messages are read back through `listMessages` like any other chat's.
 *
 * `fromMessageId` must be ON the source chat's active path; anything else is
 * rejected with code `invalid_fork_point` rather than silently reinterpreted.
 * A fork point on a branch nobody is looking at is almost always a client that
 * has gone stale, and copying the path the SERVER thinks is active would hand
 * that client a conversation it never saw.
 *
 * What lands in the copy is the active path UP TO AND INCLUDING that message,
 * flattened into a fresh straight line: new ids, no link to the runs that
 * produced the originals, and an answer still streaming (a `placeholder`) left
 * behind, because an unfinished reply is not history. Replay-only records
 * (`internal: true`) ARE copied — a fork that dropped them would show the model
 * tool results it never asked for on the next turn.
 */
export const ForkChatRequestSchema = Type.Object({
  fromMessageId: Type.String(),
});
export type ForkChatRequest = Static<typeof ForkChatRequestSchema>;

/** Body of `approveProposal` / `rejectProposal`. */
export const ProposalDecisionRequestSchema = Type.Object({
  reason: Type.Optional(Type.String()),
});
export type ProposalDecisionRequest = Static<
  typeof ProposalDecisionRequestSchema
>;

/**
 * Body of `applyProposal`. `operationId` is supplied by the CLIENT and is the
 * idempotency key for the side effect: replaying the same id returns the
 * recorded outcome instead of applying the write a second time.
 */
export const ApplyProposalRequestSchema = Type.Object({
  operationId: Type.String(),
});
export type ApplyProposalRequest = Static<typeof ApplyProposalRequestSchema>;

/**
 * Body of `updateChat`. Every field is optional; sending none is a no-op that
 * still answers with the chat, so a client can use it as a touch.
 *
 * `metadata` REPLACES the stored bag rather than merging into it — the host
 * port's rule, restated: a merge makes "unset this flag" unexpressible, and a
 * caller that wanted one already has the record it read to build it from.
 */
export const UpdateChatRequestSchema = Type.Object({
  title: Type.Optional(Type.String()),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
  archived: Type.Optional(Type.Boolean()),
});
export type UpdateChatRequest = Static<typeof UpdateChatRequestSchema>;

/**
 * Body of `regenerateMessage`. Carry an `Idempotency-Key` header with it.
 *
 * There is no `content`: that is the whole difference between this and a branch
 * `submitMessage`. The question is the one already in the chat — the target
 * message's parent — and it is not rewritten, not copied and not deleted. The
 * old answer keeps its id and its `branchIndex` and simply stops being active,
 * so a user who prefers it switches back with `activateBranch`.
 *
 * `metadata` decorates the new placeholder. The host's reserved `placeholder`
 * flag is written over anything sent here — a client cannot talk the server out
 * of marking an unfinished answer unfinished.
 */
export const RegenerateMessageRequestSchema = Type.Object({
  /** Overrides the provider's default model for this attempt only. */
  model: Type.Optional(Type.String()),
  /** Answer with a DIFFERENT provider than the one that answered first. */
  providerId: Type.Optional(Type.String()),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type RegenerateMessageRequest = Static<
  typeof RegenerateMessageRequestSchema
>;

/**
 * Body of `createProvider`. The server mints the `id` when one is not given.
 *
 * `apiKey` is WRITE-ONLY and never comes back: the server hands it to its
 * `SecretStore` under a ref and records the REF on the config, so the key
 * itself exists in exactly one place that was built to hold one. It is also the
 * reason this is a body field rather than a header — a credential in a URL or a
 * custom header ends up in an access log, and this one ends up in a store.
 *
 * `extraHeaders` is accepted and, like the key, never published back: it is
 * where a gateway's own token routinely lives.
 */
export const CreateProviderRequestSchema = Type.Object({
  id: Type.Optional(Type.String()),
  label: Type.String(),
  kind: AiProviderKindSchema,
  baseUrl: Type.String(),
  defaultModel: Type.String(),
  enabled: Type.Optional(Type.Boolean()),
  /** WRITE-ONLY. Stored through the host's `SecretStore`; never returned. */
  apiKey: Type.Optional(Type.String()),
  extraHeaders: Type.Optional(Type.Record(Type.String(), Type.String())),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type CreateProviderRequest = Static<typeof CreateProviderRequestSchema>;

/**
 * Body of `updateProvider`: the same fields, all optional, applied over the
 * stored config. `id` is absent — a provider's id is the handle other records
 * point at, and renaming it would be a create plus a delete wearing one verb.
 */
export const UpdateProviderRequestSchema = Type.Object({
  label: Type.Optional(Type.String()),
  kind: Type.Optional(AiProviderKindSchema),
  baseUrl: Type.Optional(Type.String()),
  defaultModel: Type.Optional(Type.String()),
  enabled: Type.Optional(Type.Boolean()),
  /** WRITE-ONLY, as on create. Sending it replaces the stored credential. */
  apiKey: Type.Optional(Type.String()),
  extraHeaders: Type.Optional(Type.Record(Type.String(), Type.String())),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type UpdateProviderRequest = Static<typeof UpdateProviderRequestSchema>;

/**
 * What `testProvider` answers. `ok: false` is a 200, not a 4xx: the REQUEST
 * succeeded — the server probed the endpoint and is reporting what it found —
 * and an HTTP error would make "the provider is unreachable" indistinguishable
 * from "your test request was malformed".
 */
export const TestProviderResponseSchema = Type.Object({
  ok: Type.Boolean(),
  /** Why the probe failed, when it did. Safe to show; never a credential. */
  error: Type.Optional(Type.String()),
});
export type TestProviderResponse = Static<typeof TestProviderResponseSchema>;

/**
 * Body of `updateSettings`: a partial {@link SettingsDtoSchema}, applied over
 * the single settings row, answering with the row as it now stands.
 *
 * `metadata` REPLACES, same rule as everywhere else in this contract.
 */
export const UpdateSettingsRequestSchema = Type.Partial(SettingsDtoSchema);
export type UpdateSettingsRequest = Static<typeof UpdateSettingsRequestSchema>;

/**
 * Body of `grantAllowance` — a standing "yes" for one `(chat, tool, kind)` up
 * to `maxRisk`.
 *
 * The chat is in the BODY rather than the path because the route is rooted at
 * the policy, not at a conversation: a grant is a statement about the policy's
 * contents, and `listAllowances`/`revokeAllowance` name their chat the same way
 * (a `?chatId=` query) for the same reason.
 */
export const GrantAllowanceRequestSchema = Type.Object({
  chatId: Type.String(),
  toolName: Type.String(),
  proposalKind: Type.String(),
  maxRisk: RiskLevelDtoSchema,
});
export type GrantAllowanceRequest = Static<typeof GrantAllowanceRequestSchema>;

/**
 * Body of `createMcpServer`. The server mints `id`, `createdAt` and
 * `updatedAt`; the client owns everything else.
 *
 * `alias` must be unique — it is the tool namespace, and two servers sharing one
 * would mint the same canonical id for two different tools. A duplicate is
 * refused (409 `duplicate_alias`) rather than accepted and discovered at the
 * first run that stages both servers' tools.
 */
export const CreateMcpServerRequestSchema = Type.Object({
  alias: Type.String(),
  transport: McpTransportDtoSchema,
  secretRefs: Type.Optional(Type.Record(Type.String(), Type.String())),
  enabled: Type.Optional(Type.Boolean()),
  toolAliases: Type.Optional(Type.Record(Type.String(), Type.String())),
  resilience: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type CreateMcpServerRequest = Static<
  typeof CreateMcpServerRequestSchema
>;

/**
 * Body of `updateMcpServer`: the same fields, all optional.
 *
 * FIELD-LEVEL REPLACE — a present `env`, `headers`, `secretRefs` or
 * `toolAliases` replaces the stored bag wholesale, because a merge makes
 * "remove this variable" unexpressible.
 */
export const UpdateMcpServerRequestSchema = Type.Partial(
  CreateMcpServerRequestSchema,
);
export type UpdateMcpServerRequest = Static<
  typeof UpdateMcpServerRequestSchema
>;

/**
 * What `getVersion` answers. Both versions are reported because they move
 * independently: `contractVersion` is the DTO/event shape, `restApiVersion` the
 * URL surface. `packages` is an optional map of package name → version, for a
 * deployment that wants to make its build identifiable.
 */
export const VersionDtoSchema = Type.Object({
  contractVersion: Type.String(),
  restApiVersion: Type.String(),
  packages: Type.Optional(Type.Record(Type.String(), Type.String())),
});
export type VersionDto = Static<typeof VersionDtoSchema>;

/**
 * RFC 7807 `application/problem+json` — the single error shape for every route.
 *
 * `code` is the AgentKit extension member: the stable machine-readable code the
 * host errors already carry (`lease_lost`, `duplicate_action_id`,
 * `revision_conflict`, `invalid_task_transition`, …). `type`/`title` are for
 * humans and documentation; a client branches on `code`.
 */
export const ProblemDetailsDtoSchema = Type.Object({
  type: Type.String({ description: "URI identifying the problem type." }),
  title: Type.String(),
  status: Type.Number({
    description: "HTTP status code, repeated in the body.",
  }),
  detail: Type.Optional(Type.String()),
  instance: Type.Optional(Type.String()),
  code: Type.Optional(
    Type.String({
      description: "Stable machine-readable AgentKit error code.",
    }),
  ),
});
export type ProblemDetailsDto = Static<typeof ProblemDetailsDtoSchema>;

/**
 * The body of one `streamRun` SSE frame: an `AiRunEvent` verbatim, by reference
 * to the event schema rather than a re-declaration — the stream is the event
 * contract, and a REST-flavoured copy of it would be a second definition to keep
 * in step.
 */
export const RunEventFrameDtoSchema = AiRunEventSchema;
export type RunEventFrameDto = AiRunEvent;
