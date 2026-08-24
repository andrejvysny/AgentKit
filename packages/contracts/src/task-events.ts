import { Type, type Static } from "@sinclair/typebox";

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
  },
  {
    additionalProperties: true,
    description:
      "Minimal durable task-event shape: ordered by seq, deduplicated by eventId.",
  },
);
export type TaskEventEnvelope = Static<typeof TaskEventEnvelopeSchema>;
