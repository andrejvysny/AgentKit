import { Type, type Static } from "@sinclair/typebox";
import { AiSourceRefSchema } from "./source-ref.js";
import { AiToolCallSchema } from "./tool.js";

export const AiRunEventTypeSchema = Type.Union([
  Type.Literal("run.started"),
  Type.Literal("run.message.delta"),
  Type.Literal("run.message.completed"),
  Type.Literal("run.tool.requested"),
  Type.Literal("run.tool.running"),
  Type.Literal("run.tool.succeeded"),
  Type.Literal("run.tool.failed"),
  Type.Literal("run.warning"),
  Type.Literal("run.completed"),
  Type.Literal("run.failed"),
  Type.Literal("run.cancelled"),
]);
export type AiRunEventType = Static<typeof AiRunEventTypeSchema>;

export const AiRunEventBaseSchema = Type.Object({
  type: AiRunEventTypeSchema,
  runId: Type.String(),
  timestamp: Type.String(),
});
export type AiRunEventBase = Static<typeof AiRunEventBaseSchema>;

export const AiRunStartedEventSchema = Type.Object({
  type: Type.Literal("run.started"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    model: Type.String(),
    toolCount: Type.Number(),
  }),
});
export type AiRunStartedEvent = Static<typeof AiRunStartedEventSchema>;

export const AiRunMessageDeltaEventSchema = Type.Object({
  type: Type.Literal("run.message.delta"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({ delta: Type.String() }),
});
export type AiRunMessageDeltaEvent = Static<
  typeof AiRunMessageDeltaEventSchema
>;

export const AiRunMessageCompletedEventSchema = Type.Object({
  type: Type.Literal("run.message.completed"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    content: Type.String(),
    toolCallCount: Type.Number(),
    toolCalls: Type.Optional(Type.Array(AiToolCallSchema)),
    reasoningContent: Type.Optional(
      Type.String({
        description:
          "Chain-of-thought from reasoning models (OpenAI `reasoning_content`).",
      }),
    ),
    finishReason: Type.Optional(
      Type.String({
        description:
          'Raw provider finish_reason for this turn (e.g. "stop", "length", "tool_calls").',
      }),
    ),
  }),
});
export type AiRunMessageCompletedEvent = Static<
  typeof AiRunMessageCompletedEventSchema
>;

export const AiRunToolRequestedEventSchema = Type.Object({
  type: Type.Literal("run.tool.requested"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    toolCallId: Type.String(),
    toolName: Type.String(),
    argumentsJson: Type.String(),
  }),
});
export type AiRunToolRequestedEvent = Static<
  typeof AiRunToolRequestedEventSchema
>;

export const AiRunToolRunningEventSchema = Type.Object({
  type: Type.Literal("run.tool.running"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    toolCallId: Type.String(),
    toolName: Type.String(),
  }),
});
export type AiRunToolRunningEvent = Static<typeof AiRunToolRunningEventSchema>;

export const AiRunToolSucceededEventSchema = Type.Object({
  type: Type.Literal("run.tool.succeeded"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    toolCallId: Type.String(),
    toolName: Type.String(),
    resultJson: Type.String(),
    sources: Type.Array(AiSourceRefSchema),
    truncated: Type.Boolean(),
    warnings: Type.Array(Type.String()),
    /** Tool-reported status (Track A/D). Distinguishes a partial apply from a clean success. */
    status: Type.Optional(
      Type.Union([Type.Literal("ok"), Type.Literal("partial")]),
    ),
    /** Short human/model-facing line summarising the result (Track A/D). */
    summary: Type.Optional(Type.String()),
    /** Slim payload fed to the model (Wave 0 §0.2). Full payload stays in `resultJson` for UI. */
    modelResultJson: Type.Optional(
      Type.String({
        description:
          "Slim payload fed to the model (Wave 0 §0.2). Full payload stays in resultJson for UI.",
      }),
    ),
  }),
});
export type AiRunToolSucceededEvent = Static<
  typeof AiRunToolSucceededEventSchema
>;

export const AiRunToolFailedEventSchema = Type.Object({
  type: Type.Literal("run.tool.failed"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    toolCallId: Type.String(),
    toolName: Type.String(),
    errorMessage: Type.String(),
    errorCode: Type.Optional(Type.String()),
    /**
     * #3: when a tool RAN and reported failure (esp. status:"partial"), the
     * balanced model envelope is carried here so consumers persist/replay it
     * faithfully instead of a generic error. Absent for pre-execution failures
     * (tool_missing/bad_args/schema_invalid/cap/cancelled) and exec throws.
     */
    modelResultJson: Type.Optional(
      Type.String({
        description:
          "When a tool RAN and reported failure, the balanced model envelope is carried here so consumers persist/replay it faithfully. Absent for pre-execution failures and exec throws.",
      }),
    ),
    status: Type.Optional(
      Type.Union([Type.Literal("error"), Type.Literal("partial")]),
    ),
  }),
});
export type AiRunToolFailedEvent = Static<typeof AiRunToolFailedEventSchema>;

/**
 * Known `run.warning` codes. `code` stays an open string on the wire (the schema
 * types it as `Type.String()`) for forward-compat; this union is the recognised
 * vocabulary.
 *
 * - `tool_call_cap` — a turn produced more tool calls than
 *   `maxToolCallsPerIteration`; the excess was truncated. Also the `errorCode`
 *   on the `run.tool.failed` events emitted for the skipped calls.
 * - `tool_call_unparseable` — the provider reported `finish_reason=tool_calls`
 *   but no usable tool call could be reconstructed (truncated or garbled tool
 *   block). Emitted by the provider during streaming, and by the run-loop as a
 *   fallback for providers that don't.
 * - `truncated` — `finish_reason=length`; the answer was cut off.
 * - `max_iterations` — `maxToolIterations` was reached without a final answer.
 * - `sse_parse` — malformed SSE lines were dropped and the response is likely
 *   incomplete.
 * - `empty_response`, `emulated_tool_call` — host-emitted. The runtime does not
 *   produce them, but hosts layering their own provider adapters on top of this
 *   contract do, so they are part of the shared vocabulary.
 *
 * Removed: `duplicate_loop` | `stall` | `tool_cap` | `timeout`. They were
 * declared as "P6, not emitted this iteration" and never were; reserved for
 * whoever implements those detectors.
 */
export type AiRunWarningCode =
  | "tool_call_cap"
  | "tool_call_unparseable"
  | "truncated"
  | "max_iterations"
  | "sse_parse"
  | "empty_response"
  | "emulated_tool_call";

export const AiRunWarningEventSchema = Type.Object({
  type: Type.Literal("run.warning"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    code: Type.String({
      description:
        "Open warning code; see AiRunWarningCode for the recognised vocabulary.",
    }),
    message: Type.String(),
  }),
});

/**
 * DIVERGENCE (hybrid): the wire schema keeps `data.code` an open `Type.String()`
 * (forward-compat: an unknown code must still validate), but the TS type narrows it
 * to `AiRunWarningCode | (string & {})` so editors complete the known vocabulary
 * while still accepting any string. `Static<>` cannot express that "closed union,
 * open at the type level" trick, so `data` is re-declared over the schema's static.
 */
export type AiRunWarningEvent = Omit<
  Static<typeof AiRunWarningEventSchema>,
  "data"
> & {
  data: { code: AiRunWarningCode | (string & {}); message: string };
};

export const AiRunCompletedEventSchema = Type.Object({
  type: Type.Literal("run.completed"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    iterations: Type.Number(),
    finishReason: Type.Optional(Type.String()),
  }),
});
export type AiRunCompletedEvent = Static<typeof AiRunCompletedEventSchema>;

export const AiRunFailedEventSchema = Type.Object({
  type: Type.Literal("run.failed"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({
    errorMessage: Type.String(),
    errorCode: Type.Optional(Type.String()),
  }),
});
export type AiRunFailedEvent = Static<typeof AiRunFailedEventSchema>;

export const AiRunCancelledEventSchema = Type.Object({
  type: Type.Literal("run.cancelled"),
  runId: Type.String(),
  timestamp: Type.String(),
  data: Type.Object({ reason: Type.Optional(Type.String()) }),
});
export type AiRunCancelledEvent = Static<typeof AiRunCancelledEventSchema>;

export const AiRunEventSchema = Type.Union([
  AiRunStartedEventSchema,
  AiRunMessageDeltaEventSchema,
  AiRunMessageCompletedEventSchema,
  AiRunToolRequestedEventSchema,
  AiRunToolRunningEventSchema,
  AiRunToolSucceededEventSchema,
  AiRunToolFailedEventSchema,
  AiRunWarningEventSchema,
  AiRunCompletedEventSchema,
  AiRunFailedEventSchema,
  AiRunCancelledEventSchema,
]);

/**
 * DIVERGENCE (hybrid): spelled out rather than `Static<typeof AiRunEventSchema>` so
 * the member list carries the layered {@link AiRunWarningEvent} (with its narrowed
 * `code` type) instead of the schema's raw `code: string`. Member-for-member
 * identical to `AiRunEventSchema` otherwise.
 */
export type AiRunEvent =
  | AiRunStartedEvent
  | AiRunMessageDeltaEvent
  | AiRunMessageCompletedEvent
  | AiRunToolRequestedEvent
  | AiRunToolRunningEvent
  | AiRunToolSucceededEvent
  | AiRunToolFailedEvent
  | AiRunWarningEvent
  | AiRunCompletedEvent
  | AiRunFailedEvent
  | AiRunCancelledEvent;
