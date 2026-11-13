import { Type, type Static } from "@sinclair/typebox";

/**
 * AGENTKIT-90 — the supported public surface for persistent teammates:
 * agent lifecycle and definition versions, durable task submission,
 * task status/history, event replay, and effective-run-profile
 * inspection.
 *
 * These DTOs MIRROR the service boundary
 * (`AgentDefinitionService`, `TeammateService` in `rust/crates/agents`,
 * `RunProfileService` in `rust/crates/sessions`) and the durable store rows
 * beneath it; they add
 * nothing the services do not already guarantee. Everything here is
 * ADDITIVE to the existing contract: no pre-existing DTO changed, and
 * every task/agent route is a new operation on the same REST v1 surface.
 *
 * The security invariants the wire has to keep:
 * - **Secret references only.** `AgentDefinitionDto.secretRefs` carries
 *   scheme-qualified REFERENCES (`vault://…`); there is no field anywhere
 *   in this module that could carry a credential VALUE, and the closed
 *   objects (`additionalProperties: false`) refuse an envelope that tries.
 * - **Unknown is unknown.** Usage counters are
 *   optional-or-{@link Type.Null} (`TaskEventUsageSchema`,
 *   `TaskEventUsageTotalsDtoSchema`): a missing counter means the
 *   provider never reported one — it must render as "unknown", never as
 *   a zero that looks like a free run.
 * - **Reads create nothing.** Every status/history/event operation here
 *   is a GET (or a replay-safe SSE resume); the only POSTs are the
 *   explicitly idempotent admission (`submitTeammateTask`, keyed by
 *   `admissionKey`) and the state transitions that mutate on purpose
 *   (archive/restore/cancel/model).
 */

import { AiSourceRefSchema } from "./source-ref.js";
import { TaskEventEnvelopeSchema } from "./task-events.js";

/** Budget ceilings MIRROR of `AgentBudgets` (`rust/crates/store/src/agents.rs`).
 * Absence means "this ceiling is unset; the host default governs". */
export const AgentBudgetsDtoSchema = Type.Object(
  {
    maxToolIterations: Type.Optional(
      Type.Number({ minimum: 1, maximum: 9_007_199_254_740_991 }),
    ),
    maxToolCallsPerIteration: Type.Optional(
      Type.Number({ minimum: 1, maximum: 9_007_199_254_740_991 }),
    ),
    maxOutputTokens: Type.Optional(
      Type.Number({ minimum: 1, maximum: 9_007_199_254_740_991 }),
    ),
    attemptBudgetMs: Type.Optional(
      Type.Number({ minimum: 1, maximum: 9_007_199_254_740_991 }),
    ),
  },
  { additionalProperties: false },
);
export type AgentBudgetsDto = Static<typeof AgentBudgetsDtoSchema>;

/** MIRROR of `AgentSkillSelection`: a skill pinned at a revision. */
export const AgentSkillSelectionDtoSchema = Type.Object(
  {
    id: Type.String(),
    revision: Type.String(),
  },
  { additionalProperties: false },
);
export type AgentSkillSelectionDto = Static<
  typeof AgentSkillSelectionDtoSchema
>;

/**
 * The effective configuration of one agent. MIRROR of `AgentDefinition`
 * (`rust/crates/store/src/agents.rs`); the closed object is the wire-side
 * half of the store's smuggling gate — a credential VALUE, a foreign
 * scope, or an unknown member is refused before the service ever sees it.
 * `permissionMode` is a CEILING (`read_only|ask|auto_edit|full_auto|plan`);
 * it narrows what a run may attempt and grants nothing by itself.
 */
export const AgentDefinitionDtoSchema = Type.Object(
  {
    name: Type.String(),
    avatar: Type.Optional(Type.String()),
    purpose: Type.String(),
    instructions: Type.String(),
    resources: Type.Array(Type.String()),
    skills: Type.Array(AgentSkillSelectionDtoSchema),
    model: Type.Optional(Type.String()),
    reasoningEffort: Type.Optional(Type.String()),
    permissionMode: Type.String(),
    allowedTools: Type.Array(Type.String()),
    budgets: AgentBudgetsDtoSchema,
    /** Secret REFERENCES only (e.g. `vault://…`); values never enter. */
    secretRefs: Type.Array(Type.String()),
  },
  { additionalProperties: false },
);
export type AgentDefinitionDto = Static<typeof AgentDefinitionDtoSchema>;

/** Lifecycle a listed agent can be in. `deleted` is a tombstone: no read
 * route ever returns it, so it is NOT in this vocabulary. MIRROR of the
 * readable half of `AgentLifecycle`. */
export const AgentLifecycleDtoSchema = Type.Union([
  Type.Literal("active"),
  Type.Literal("archived"),
]);
export type AgentLifecycleDto = Static<typeof AgentLifecycleDtoSchema>;

/** One durable agent row. Projection of `AgentRecord` minus tombstones. */
export const AgentDtoSchema = Type.Object({
  agentId: Type.String(),
  scope: Type.String(),
  ownerId: Type.String(),
  lifecycle: AgentLifecycleDtoSchema,
  currentRevision: Type.Number(),
  definition: AgentDefinitionDtoSchema,
  /** Unix milliseconds. */
  createdAt: Type.Number(),
  /** Unix milliseconds. */
  updatedAt: Type.Number(),
});
export type AgentDto = Static<typeof AgentDtoSchema>;

/** MIRROR of `AgentRevisionSource`: what produced one configuration
 * revision (`rust/crates/store/src/agents.rs`). */
export const AgentRevisionSourceDtoSchema = Type.Union([
  Type.Literal("create"),
  Type.Literal("edit"),
  Type.Literal("model_change"),
  Type.Literal("restore"),
]);
export type AgentRevisionSourceDto = Static<
  typeof AgentRevisionSourceDtoSchema
>;

/** One row of the append-only configuration ledger: the full effective
 * definition snapshot at `revision`, with its provenance. Projection of
 * `AgentRevision`. */
export const AgentRevisionDtoSchema = Type.Object({
  agentId: Type.String(),
  revision: Type.Number(),
  snapshot: AgentDefinitionDtoSchema,
  source: AgentRevisionSourceDtoSchema,
  /** Unix milliseconds. */
  createdAt: Type.Number(),
});
export type AgentRevisionDto = Static<typeof AgentRevisionDtoSchema>;

/** Body of `createAgent`. The agent's scope and owner are assigned by the
 * SERVER from the authenticated actor — the request cannot name them, so
 * an actor can never create an agent into another actor's scope. */
export const CreateAgentRequestSchema = Type.Object(
  {
    definition: AgentDefinitionDtoSchema,
  },
  { additionalProperties: false },
);
export type CreateAgentRequest = Static<typeof CreateAgentRequestSchema>;

/** Body of `updateAgent`. `expectedRevision` is the optimistic-concurrency
 * token the caller read; a stale value is a 409 `revision_conflict` that
 * applied nothing. */
export const UpdateAgentRequestSchema = Type.Object(
  {
    expectedRevision: Type.Number(),
    definition: AgentDefinitionDtoSchema,
  },
  { additionalProperties: false },
);
export type UpdateAgentRequest = Static<typeof UpdateAgentRequestSchema>;

/** Body of `setAgentModel`: move the model preference under the SAME
 * identity. Identity, scope, owner, and the whole revision ledger are
 * untouched; the append carries `model_change` provenance. */
export const SetAgentModelRequestSchema = Type.Object(
  {
    expectedRevision: Type.Number(),
    model: Type.Optional(Type.Union([Type.String(), Type.Null()])),
    reasoningEffort: Type.Optional(Type.Union([Type.String(), Type.Null()])),
  },
  { additionalProperties: false },
);
export type SetAgentModelRequest = Static<typeof SetAgentModelRequestSchema>;

/** MIRROR of `TaskProfileOverrides`
 * (`rust/crates/sessions/src/run_profiles.rs`).
 * The closed object IS the smuggling gate: there is no field for a secret
 * value, a credential, or a permission grant, and an envelope that tries
 * answers as invalid overrides. Requests can only ever NARROW (tools
 * inside the agent's ceiling, modes below it, budgets under its ceilings). */
export const TaskProfileOverridesDtoSchema = Type.Object(
  {
    providerId: Type.Optional(Type.String()),
    model: Type.Optional(Type.String()),
    tools: Type.Optional(Type.Array(Type.String())),
    requiresVision: Type.Optional(Type.Boolean()),
    requiresContinuation: Type.Optional(Type.Boolean()),
    permissionMode: Type.Optional(Type.String()),
    budgets: Type.Optional(AgentBudgetsDtoSchema),
    /** The ONLY way a context overflow resolves — an explicit,
     * on-record decision to drop whole units. Without it overflow is a
     * refusal, never a silent drop. */
    truncation: Type.Optional(
      Type.Object(
        {
          mode: Type.Literal("explicit"),
        },
        { additionalProperties: false },
      ),
    ),
  },
  { additionalProperties: false },
);
export type TaskProfileOverridesDto = Static<
  typeof TaskProfileOverridesDtoSchema
>;

/** MIRROR of `ContextUnit`: one unit of compiled context. `toolPair`
 * units (a call and its result) are ATOMIC — truncation drops whole
 * units and never splits a pair. */
export const ContextUnitDtoSchema = Type.Object(
  {
    kind: Type.Union([Type.Literal("text"), Type.Literal("toolPair")]),
    text: Type.String(),
    citations: Type.Array(Type.String()),
    required: Type.Boolean(),
  },
  { additionalProperties: false },
);
export type ContextUnitDto = Static<typeof ContextUnitDtoSchema>;

/** MIRROR of `SelectedSource`: one context source the caller wants
 * composed. Selection never GRANTS access — the agent's own definition
 * grants (skills pinned in its selection, resources in its list). */
export const SelectedSourceDtoSchema = Type.Object(
  {
    id: Type.String(),
    kind: Type.Union([Type.Literal("skill"), Type.Literal("resource")]),
    units: Type.Array(ContextUnitDtoSchema),
  },
  { additionalProperties: false },
);
export type SelectedSourceDto = Static<typeof SelectedSourceDtoSchema>;

/** MIRROR of `ApprovalGrantRef`: a standing approval the admission was
 * granted on, recorded so revocation can fence dispatch at use. */
export const ApprovalGrantRefDtoSchema = Type.Object(
  {
    sessionId: Type.String(),
    grantId: Type.String(),
    digest: Type.String(),
  },
  { additionalProperties: false },
);
export type ApprovalGrantRefDto = Static<typeof ApprovalGrantRefDtoSchema>;

/**
 * Body of `submitTeammateTask` — the ONE admission path.
 *
 * `admissionKey` is the idempotency key: the same key under the same
 * agent admits THE SAME logical task, across reconnects and process
 * restarts. A duplicate submission therefore returns the existing task
 * with `replayed: true` and creates nothing — which is why this route
 * needs no `Idempotency-Key` header: its key is in the body, part of the
 * validated contract, and survives a client that cannot set headers.
 *
 * When `taskProfile`/`projectSources`/`approvals` are present the server
 * resolves the effective run profile and pins it INSIDE the admission
 * transaction (`getAgentTaskRunProfile` then explains it); absent admits
 * without a profile.
 */
export const SubmitTeammateTaskRequestSchema = Type.Object(
  {
    /** Caller-pinned logical id (retry of a known admission); absent
     * mints one. */
    taskId: Type.Optional(Type.String()),
    admissionKey: Type.String(),
    kind: Type.String(),
    payload: Type.Record(Type.String(), Type.Unknown()),
    priority: Type.Optional(
      Type.Number({
        minimum: -9_007_199_254_740_991,
        maximum: 9_007_199_254_740_991,
      }),
    ),
    dependsOn: Type.Optional(Type.Array(Type.String())),
    parentTaskId: Type.Optional(Type.String()),
    taskProfile: Type.Optional(TaskProfileOverridesDtoSchema),
    projectSources: Type.Optional(Type.Array(SelectedSourceDtoSchema)),
    approvals: Type.Optional(Type.Array(ApprovalGrantRefDtoSchema)),
  },
  { additionalProperties: false },
);
export type SubmitTeammateTaskRequest = Static<
  typeof SubmitTeammateTaskRequestSchema
>;

/** The normalized status of an agent-bound logical task. MIRROR of
 * `TaskStatus` (`rust/crates/store/src/models.rs`, snake_case);
 * `waiting_child` and `uncertain` are the AGENTKIT-88 lane-owning parks. */
export const AgentTaskStatusDtoSchema = Type.Union([
  Type.Literal("queued"),
  Type.Literal("running"),
  Type.Literal("interrupted"),
  Type.Literal("waiting_approval"),
  Type.Literal("waiting_child"),
  Type.Literal("uncertain"),
  Type.Literal("completed"),
  Type.Literal("failed"),
  Type.Literal("cancelled"),
]);
export type AgentTaskStatusDto = Static<typeof AgentTaskStatusDtoSchema>;

/**
 * One logical task, projected. This is the STATUS/History read model: it
 * is derived from the task row — the canonical host state — on every
 * read, so it cannot drift, and a client that reconnects loads history
 * through it without creating anything.
 */
export const AgentTaskDtoSchema = Type.Object({
  taskId: Type.String(),
  /** The persistent agent identity this task runs under, written once at
   * admission. */
  agentId: Type.String(),
  /** The immutable definition revision the admission pinned. */
  agentRevision: Type.Number(),
  admissionKey: Type.String(),
  kind: Type.String(),
  status: AgentTaskStatusDtoSchema,
  priority: Type.Number(),
  /** Unix milliseconds. */
  enqueuedAt: Type.Number(),
  /** Unix milliseconds. */
  availableAt: Type.Number(),
  startedAt: Type.Optional(Type.Number()),
  finishedAt: Type.Optional(Type.Number()),
  error: Type.Optional(Type.String()),
  attemptCount: Type.Number(),
  cancelRequested: Type.Boolean(),
  dependsOn: Type.Array(Type.String()),
  parentTaskId: Type.Optional(Type.String()),
  /** sha256 of the sealed effective run profile, when the admission
   * pinned one. */
  runProfileDigest: Type.Optional(Type.String()),
});
export type AgentTaskDto = Static<typeof AgentTaskDtoSchema>;

/** Answer of `submitTeammateTask`. `replayed` distinguishes the call
 * that CREATED the logical task from a duplicate key returning the one it
 * already created — both return the same task, only one wrote it. */
export const SubmitTeammateTaskResponseSchema = Type.Object({
  task: AgentTaskDtoSchema,
  replayed: Type.Boolean(),
});
export type SubmitTeammateTaskResponse = Static<
  typeof SubmitTeammateTaskResponseSchema
>;

/** One bounded page of a task's durable event log. `nextCursor` is the
 * `seq` to pass as `?after=` for the following page; absent when the page
 * ran dry. Paging by `seq` — never by offset — is what makes a page
 * stable beside a live emitter. */
export const TaskEventPageDtoSchema = Type.Object({
  events: Type.Array(TaskEventEnvelopeSchema),
  nextCursor: Type.Optional(Type.String()),
});
export type TaskEventPageDto = Static<typeof TaskEventPageDtoSchema>;

/**
 * The bounded usage totals a task projection carries. **Unknown is
 * unknown**: a counter is `null` while no durable `run.usage` event ever
 * reported a number for it — a consumer must not render `null` as `0`.
 * Present numbers are the checked sum over the final usage events the
 * log holds.
 */
export const TaskEventUsageTotalsDtoSchema = Type.Object(
  {
    promptTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    completionTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    totalTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    cachedPromptTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    reasoningTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    /** Number of final usage events the totals were summed over. */
    usageEvents: Type.Number({ minimum: 0 }),
  },
  { additionalProperties: false },
);
export type TaskEventUsageTotalsDto = Static<
  typeof TaskEventUsageTotalsDtoSchema
>;

/**
 * The normalized approval a task projection carries: the last approval
 * state the durable log recorded, or absent when the task never parked
 * on one.
 */
export const TaskProjectionApprovalDtoSchema = Type.Object(
  {
    state: Type.Union([
      Type.Literal("requested"),
      Type.Literal("granted"),
      Type.Literal("denied"),
    ]),
    approvalId: Type.Optional(Type.String()),
    toolName: Type.Optional(Type.String()),
    /** `seq` of the event that recorded it — the ordered proof. */
    seq: Type.Number(),
  },
  { additionalProperties: false },
);
export type TaskProjectionApprovalDto = Static<
  typeof TaskProjectionApprovalDtoSchema
>;

/**
 * The SAFE BOUNDED PROJECTION of one agent-bound task (AGENTKIT-90): a
 * derived view, not a second event store. It is recomputed on every read
 * from the task row (canonical status) and a BOUNDED page of the task's
 * durable events (attempt, approval, usage totals, source references),
 * so it reconstructs identically after a process restart, mid-way
 * through a cancellation, and beside a live emitter. Credential-bearing
 * and oversized tool payload material is redacted/truncated here, before
 * anything leaves the host.
 */
export const AgentTaskProjectionDtoSchema = Type.Object({
  task: AgentTaskDtoSchema,
  /** The latest attempt the log names, when events carried one. */
  attemptId: Type.Optional(Type.String()),
  /** Highest `seq` observed in the bounded window; absent for an empty log. */
  lastSeq: Type.Optional(Type.Number()),
  /** Events read for this projection (≤ the bounded window). */
  eventCount: Type.Number({ minimum: 0 }),
  approval: Type.Optional(TaskProjectionApprovalDtoSchema),
  usage: TaskEventUsageTotalsDtoSchema,
  sourceRefs: Type.Array(AiSourceRefSchema),
});
export type AgentTaskProjectionDto = Static<
  typeof AgentTaskProjectionDtoSchema
>;

/** Answer of `getAgentTaskRunProfile`: the sealed effective profile a
 * task was admitted with, verified by digest. `digest`/`profile` are
 * absent when the admission pinned no profile (a pre-89 admission) —
 * absence is the honest answer, not an empty profile. */
export const RunProfileExplanationDtoSchema = Type.Object({
  taskId: Type.String(),
  digest: Type.Optional(Type.String()),
  profile: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type RunProfileExplanationDto = Static<
  typeof RunProfileExplanationDtoSchema
>;
