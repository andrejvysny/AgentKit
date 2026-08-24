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
 * **Two enumerations are mirrored, not imported.** `RunStatusDto` and
 * `ProposalStatusDto` (and `RiskLevelDto`) restate unions that `@agentkit/host`
 * owns, because contracts sits *below* host and cannot depend on it. They must
 * be kept in step by hand; the compile-time cross-check lives in
 * `packages/host/tests/state-machines.test.ts`, and each one names its source
 * below.
 */
import { Type, type Static } from "@sinclair/typebox";
import { AiRunEventSchema, type AiRunEvent } from "./run-events.js";
import { AiSourceRefSchema } from "./source-ref.js";
import {
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
 * - **`submitMessage`** REQUIRES an `Idempotency-Key` header. Submitting a turn
 *   creates a run and two messages; a client that retries a timed-out POST
 *   without a key duplicates the turn, and no amount of server-side cleverness
 *   can tell that retry apart from a user who really did send twice.
 * - **`streamRun`** is Server-Sent Events, and resumes on `Last-Event-ID`: the
 *   value is an `AiRunEvent.eventId`, and the server replays everything after
 *   that event from the run's durable log. This is why `eventId` and `seq` are
 *   required base fields rather than decoration.
 */
export const REST_ROUTES = {
  createChat: { method: "POST", path: "/v1/chats" },
  listChats: { method: "GET", path: "/v1/chats" },
  getChat: { method: "GET", path: "/v1/chats/:chatId" },

  listMessages: { method: "GET", path: "/v1/chats/:chatId/messages" },
  /** Requires an `Idempotency-Key` header; see the note above. */
  submitMessage: { method: "POST", path: "/v1/chats/:chatId/messages" },

  getRun: { method: "GET", path: "/v1/runs/:runId" },
  /** SSE. Resumes from `Last-Event-ID` (an `AiRunEvent.eventId`). */
  streamRun: { method: "GET", path: "/v1/runs/:runId/stream" },
  cancelRun: { method: "POST", path: "/v1/runs/:runId/cancel" },

  listToolEvents: { method: "GET", path: "/v1/chats/:chatId/tool-events" },

  listProposals: { method: "GET", path: "/v1/chats/:chatId/proposals" },
  approveProposal: { method: "POST", path: "/v1/proposals/:proposalId/approve" },
  rejectProposal: { method: "POST", path: "/v1/proposals/:proposalId/reject" },
  applyProposal: { method: "POST", path: "/v1/proposals/:proposalId/apply" },

  listProviders: { method: "GET", path: "/v1/providers" },
  listModels: { method: "GET", path: "/v1/providers/:providerId/models" },
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

// ---------------------------------------------------------------------------
// Resource DTOs
// ---------------------------------------------------------------------------

/** A conversation. Projection of `ChatRecord`. */
export const ChatDtoSchema = Type.Object({
  id: Type.String(),
  title: Type.Optional(Type.String()),
  createdAt: Type.String({ description: "ISO-8601." }),
  updatedAt: Type.String({ description: "ISO-8601." }),
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
  content: Type.String(),
  toolCalls: Type.Optional(Type.Array(AiToolCallSchema)),
  toolCallId: Type.Optional(Type.String()),
  metadata: Type.Record(Type.String(), Type.Unknown()),
  createdAt: Type.String({ description: "ISO-8601." }),
});
export type MessageDto = Static<typeof MessageDtoSchema>;

/**
 * One page of messages, oldest first. `nextCursor` is opaque: it encodes the
 * store's ordering key, and a client that tried to interpret it would be reading
 * an implementation detail this contract does not promise.
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
  content: Type.String(),
  /** Overrides the provider's default model for this turn only. */
  model: Type.Optional(Type.String()),
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
  status: Type.Number({ description: "HTTP status code, repeated in the body." }),
  detail: Type.Optional(Type.String()),
  instance: Type.Optional(Type.String()),
  code: Type.Optional(
    Type.String({ description: "Stable machine-readable AgentKit error code." }),
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
