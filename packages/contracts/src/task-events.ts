import { Type, type Static } from "@sinclair/typebox";

import { AiSourceRefSchema } from "./source-ref.js";

/**
 * The NORMALIZED status an extended task event may carry (AGENTKIT-90):
 * the durable task vocabulary, mirrored from `TaskStatus`
 * (`rust/crates/store/src/models.rs`, snake_case on the wire). It is a
 * self-description of what the emitter knew when the event was written —
 * the task ROW remains the canonical status; a projection that must agree
 * with the host reads the row, and the event field is the ordered,
 * replay-safe hint that lets a consumer follow transitions live.
 */
export const TaskEventStatusSchema = Type.Union([
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
export type TaskEventStatus = Static<typeof TaskEventStatusSchema>;

/**
 * The normalized approval view one event may carry: what approval the
 * run is parked on, and what became of it. Open so a host can add its own
 * members; `state` is the closed part a consumer branches on.
 */
export const TaskEventApprovalSchema = Type.Object(
  {
    state: Type.Union([
      Type.Literal("requested"),
      Type.Literal("granted"),
      Type.Literal("denied"),
    ]),
    approvalId: Type.Optional(Type.String()),
    toolName: Type.Optional(Type.String()),
  },
  { additionalProperties: true },
);
export type TaskEventApproval = Static<typeof TaskEventApprovalSchema>;

/**
 * The normalized usage view one event may carry. **Absent is not zero**:
 * every counter is optional-or-null, and a consumer must render `null`
 * ("the provider did not say") differently from `0` ("the provider said
 * none"). A projection sums only over present numbers and stays `null`
 * while no event has ever reported a number for that counter — an
 * unknown cost is unknown, never a reassuring `0`.
 */
export const TaskEventUsageSchema = Type.Object(
  {
    callId: Type.Optional(Type.String()),
    model: Type.Optional(Type.String()),
    promptTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    completionTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    totalTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    cachedPromptTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
    reasoningTokens: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
  },
  { additionalProperties: true },
);
export type TaskEventUsage = Static<typeof TaskEventUsageSchema>;

/**
 * The minimal shape a durable task-event log stores and orders.
 *
 * A chat turn is one kind of task; its event vocabulary is `AiRunEvent`
 * (`./run-events.ts`), and every member of that union structurally satisfies
 * this envelope. Other task kinds — an indexing job, a batch apply — specialize
 * the envelope with their own `type` vocabulary and their own `data`. The store
 * underneath them does not care which: it **orders by `seq`**, **dedups by
 * `eventId`**, and reads nothing else. `additionalProperties` is therefore open
 * — `data`, `runId`, and whatever else a vocabulary adds ride through
 * untouched.
 *
 * - `type` — open here on purpose; the per-task-kind vocabulary closes it.
 * - `seq` — strictly increasing within one task. The real ordering key; a
 *   consumer can detect a gap (dropped event) or a reorder, which timestamps
 *   cannot show.
 * - `eventId` — unique per event. What deduplication and acknowledgement key on
 *   when a stream is replayed or re-delivered.
 * - `timestamp` — ISO-8601 emission time. Wall-clock, therefore NOT an ordering
 *   key: two events can share a millisecond, and clocks move.
 * - `contractVersion` — the {@link CONTRACT_VERSION} the emitter spoke, so a
 *   consumer reading a persisted log knows which shape it is looking at instead
 *   of guessing from the fields present.
 * - `attemptId` — optional; groups the events of one attempt when a task is
 *   retried, so a replay of attempt 2 is distinguishable from attempt 1.
 */
export const TaskEventEnvelopeSchema = Type.Object(
  {
    type: Type.String({
      description:
        "Event type. Open here; the per-task-kind vocabulary closes it.",
    }),
    seq: Type.Number({
      description: "Strictly increasing within a task; the ordering key.",
    }),
    eventId: Type.String({
      description: "Unique per event; the key for dedup on replay.",
    }),
    timestamp: Type.String({
      description: "ISO-8601 emission time. Not an ordering key — use `seq`.",
    }),
    contractVersion: Type.String({
      description: "CONTRACT_VERSION the emitter spoke.",
    }),
    attemptId: Type.Optional(
      Type.String({
        description: "Groups the events of one attempt when a task is retried.",
      }),
    ),
    // --- AGENTKIT-90 identity & normalized projections -----------------
    // Additive members: an emitter that predates them stays valid, and a
    // consumer treats absence as "the emitter did not say", never as a
    // default. The store stamps `agentId`/`taskId`/`definitionRevision`
    // (and an `attemptId` the emitter left out) at the single append
    // choke-point, so every event of an agent-bound task carries its full
    // identity no matter which emitter wrote it.
    agentId: Type.Optional(
      Type.String({
        description: "Durable agent identity the event's task runs under.",
      }),
    ),
    taskId: Type.Optional(
      Type.String({
        description: "Logical task the event belongs to (the log's key).",
      }),
    ),
    definitionRevision: Type.Optional(
      Type.Number({
        description:
          "Agent configuration revision pinned by the task's admission.",
      }),
    ),
    status: Type.Optional(TaskEventStatusSchema),
    approval: Type.Optional(TaskEventApprovalSchema),
    usage: Type.Optional(TaskEventUsageSchema),
    sourceRefs: Type.Optional(
      Type.Array(AiSourceRefSchema, {
        description:
          "Normalized source references the event introduced or used.",
      }),
    ),
  },
  {
    additionalProperties: true,
    description:
      "Minimal durable task-event shape: ordered by seq, deduplicated by eventId.",
  },
);
export type TaskEventEnvelope = Static<typeof TaskEventEnvelopeSchema>;
