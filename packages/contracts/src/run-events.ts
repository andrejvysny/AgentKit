import { Type, type Static } from "@sinclair/typebox";
import { AiSourceRefSchema } from "./source-ref.js";
import { AiToolCallSchema } from "./tool.js";

export const AiRunEventTypeSchema = Type.Union([
  Type.Literal("run.started"),
  Type.Literal("run.message.delta"),
  Type.Literal("run.message.reasoning-delta"),
  Type.Literal("run.message.completed"),
  Type.Literal("run.tool.requested"),
  Type.Literal("run.tool.running"),
  Type.Literal("run.tool.succeeded"),
  Type.Literal("run.tool.failed"),
  Type.Literal("run.warning"),
  Type.Literal("run.usage"),
  Type.Literal("run.context.compacted"),
  Type.Literal("run.verification"),
  Type.Literal("run.input.applied"),
  Type.Literal("run.waiting"),
  Type.Literal("run.completed"),
  Type.Literal("run.failed"),
  Type.Literal("run.cancelled"),
]);
export type AiRunEventType = Static<typeof AiRunEventTypeSchema>;

/**
 * Transport-level fields carried by EVERY run event, independent of what the
 * event says.
 *
 * - `runId` — the run these events belong to.
 * - `timestamp` — ISO-8601 emission time. Wall-clock, therefore NOT an ordering
 *   key: two events can share a millisecond, and clocks move.
 * - `contractVersion` — the {@link CONTRACT_VERSION} the emitter spoke, so a
 *   consumer reading a persisted stream knows which shape it is looking at
 *   instead of guessing from the fields present.
 * - `eventId` — unique per event. What deduplication and acknowledgement key on
 *   when a stream is replayed or re-delivered.
 * - `seq` — strictly increasing within a run. The real ordering key; a consumer
 *   can detect a gap (dropped event) or a reorder, which timestamps cannot show.
 * - `attemptId` — optional; groups the events of one attempt when a run is
 *   retried, so a replay of attempt 2 is distinguishable from attempt 1.
 */
const runEventBaseFields = {
  runId: Type.String(),
  timestamp: Type.String({
    description: "ISO-8601 emission time. Not an ordering key — use `seq`.",
  }),
  contractVersion: Type.String({
    description: "CONTRACT_VERSION the emitter spoke.",
  }),
  eventId: Type.String({
    description: "Unique per event; the key for dedup on replay.",
  }),
  seq: Type.Number({
    description: "Strictly increasing within a run; the ordering key.",
  }),
  attemptId: Type.Optional(
    Type.String({
      description: "Groups the events of one attempt when a run is retried.",
    }),
  ),
};

export const AiRunEventBaseSchema = Type.Object({
  type: AiRunEventTypeSchema,
  ...runEventBaseFields,
});
export type AiRunEventBase = Static<typeof AiRunEventBaseSchema>;

export const AiRunStartedEventSchema = Type.Object({
  type: Type.Literal("run.started"),
  ...runEventBaseFields,
  data: Type.Object({
    model: Type.String(),
    toolCount: Type.Number(),
  }),
});
export type AiRunStartedEvent = Static<typeof AiRunStartedEventSchema>;

export const AiRunMessageDeltaEventSchema = Type.Object({
  type: Type.Literal("run.message.delta"),
  ...runEventBaseFields,
  data: Type.Object({ delta: Type.String() }),
});
export type AiRunMessageDeltaEvent = Static<
  typeof AiRunMessageDeltaEventSchema
>;

/** Provider-supplied public reasoning text, separate from the visible answer. */
export const AiRunMessageReasoningDeltaEventSchema = Type.Object({
  type: Type.Literal("run.message.reasoning-delta"),
  ...runEventBaseFields,
  data: Type.Object({ delta: Type.String() }),
});
export type AiRunMessageReasoningDeltaEvent = Static<
  typeof AiRunMessageReasoningDeltaEventSchema
>;

export const AiRunMessageCompletedEventSchema = Type.Object({
  type: Type.Literal("run.message.completed"),
  ...runEventBaseFields,
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
          'Raw provider finish_reason for this turn (e.g. "stop", "length", "tool_calls"), or the synthetic "incomplete" when the stream ended without one (see the stream_incomplete warning).',
      }),
    ),
  }),
});
export type AiRunMessageCompletedEvent = Static<
  typeof AiRunMessageCompletedEventSchema
>;

export const AiRunToolRequestedEventSchema = Type.Object({
  type: Type.Literal("run.tool.requested"),
  ...runEventBaseFields,
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
  ...runEventBaseFields,
  data: Type.Object({
    toolCallId: Type.String(),
    toolName: Type.String(),
  }),
});
export type AiRunToolRunningEvent = Static<typeof AiRunToolRunningEventSchema>;

export const AiRunToolSucceededEventSchema = Type.Object({
  type: Type.Literal("run.tool.succeeded"),
  ...runEventBaseFields,
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
  ...runEventBaseFields,
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
 * - `multimodal_flattened` — a message on a role that cannot carry content
 *   parts (`system`/`tool`) arrived with them; the provider client flattened the
 *   text parts and dropped the image parts rather than failing the request.
 * - `attachment_unresolved` — an image part carried a `ref` source (see
 *   `AiImageSourceSchema`) that the host could not turn into bytes for this
 *   pass: no resolver is wired, or the resolver answered "gone". The part was
 *   dropped from what the provider was shown; the STORED message keeps the ref,
 *   so a later turn can try again.
 * - `attachment_budget_exceeded` — a `ref` image resolved, but sending it would
 *   have blown the pass's attachment budget (this image's own byte cap, the
 *   run's total bytes, or its image count). Dropped from the pass, kept in the
 *   store, same as above.
 * - `stream_incomplete` — the provider's stream ended without a `[DONE]`
 *   sentinel and without any `finish_reason` (an idle proxy cut, a dropped
 *   socket). What arrived is a partial answer; the turn's `finishReason` is
 *   `"incomplete"` rather than the `"stop"` that would claim it finished.
 * - `result_unserializable` — a tool RAN, but its result could not be turned
 *   into JSON (a BigInt, a cycle), so the model was fed a failure envelope
 *   instead. Its side effects still happened; only the reporting failed. Also
 *   the `errorCode` on the matching `run.tool.failed`.
 * - `duplicate_tool_call_id` — one streamed turn produced two tool calls with
 *   the same id. The later ones are re-keyed (`<id>#2`, `<id>#3`) so every call
 *   keeps a distinct, answerable `tool_call_id`.
 * - `tool_late_result` — reserved: a tool that ignored its deadline settled
 *   after the run had already reported it as timed out. Not emitted today (the
 *   late result is dropped without an event).
 * - `retry_pass` — host-emitted immediately BEFORE a recovery or correction
 *   pass. A run is NOT one pass: the host re-asks after a failed pass
 *   (`chat_only`), after a completed-but-empty one (`empty_response`), and once
 *   more per correction round (`correction`), and each pass writes its own
 *   `run.started` … terminal pair onto the same log. This warning is the
 *   boundary between them: `data.pass` is the 1-based number of the pass about
 *   to run and `data.reason` is `"chat_only" | "empty_response" |
 *   "correction"`. A consumer must treat the run as LIVE again — the terminal
 *   event it just saw belonged to the previous pass — and RESET the text it has
 *   streamed so far, mirroring the host's own reset of the stored answer.
 * - `hook_timeout` — host-emitted: one of the host's own hooks
 *   (`ContextProvider`, `AttachmentResolver`, `ToolSetContributor.contribute`,
 *   `VerificationHook`) did not answer inside its deadline, so the turn went on
 *   WITHOUT what it would have contributed — no bindings, no system prompt, one
 *   contributor's tools missing. The turn is degraded, not failed, and the hook
 *   may still settle later; its answer is discarded. Also core-emitted: a
 *   `runAgentLoop` hook (ADR 0015 §6) that exceeded its deadline.
 * - `hook_failed` — core-emitted: a `runAgentLoop` observer hook threw
 *   (ADR 0015 §6); the run went on without it. Like `hook_timeout`, it is also
 *   emitted by core loop hooks. The message names the hook, never the thrown
 *   text.
 * - `context_budget_fallback` — host-emitted on an agent-session turn: nothing
 *   says how large the model's context window is (no session override, model
 *   record or provider capability), so the budget assumed 32768 tokens.
 * - `context_trimmed` — host-emitted on an agent-session turn: the transcript
 *   was over the window budget and the oldest whole turns were left out of this
 *   pass. The message carries the turn count and the estimated tokens.
 * - `compaction_failed` — host-emitted on an agent-session turn: the context
 *   summarizer failed (provider error, empty summary), so this pass used the
 *   plain `context_trimmed` fallback instead. The message carries no model text.
 * - `skill_unavailable` — host-emitted on an agent-session turn: a Skill the
 *   session carries over could not be activated (missing, or its revision or
 *   hash changed). The turn went on without it; the message names the skill id.
 * - `project_instructions_unavailable` — host-emitted on an agent-session turn:
 *   an `AGENTS.md` / `CLAUDE.md` could not be read (symlink, outside the root)
 *   or exceeded its size cap and was truncated.
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
  | "multimodal_flattened"
  | "attachment_unresolved"
  | "attachment_budget_exceeded"
  | "stream_incomplete"
  | "result_unserializable"
  | "duplicate_tool_call_id"
  | "tool_late_result"
  | "retry_pass"
  | "hook_timeout"
  | "hook_failed"
  | "empty_response"
  | "emulated_tool_call"
  | "context_budget_fallback"
  | "context_trimmed"
  | "compaction_failed"
  | "skill_unavailable"
  | "project_instructions_unavailable";

export const AiRunWarningEventSchema = Type.Object({
  type: Type.Literal("run.warning"),
  ...runEventBaseFields,
  data: Type.Object({
    code: Type.String({
      description:
        "Open warning code; see AiRunWarningCode for the recognised vocabulary.",
    }),
    message: Type.String(),
    // Both optional and both open, for the same reason `code` is: a warning
    // that carries no pass context is still a valid warning, and a code this
    // consumer has never heard of may carry fields it has never heard of.
    pass: Type.Optional(
      Type.Number({
        description:
          "1-based pass this warning is about; set by retry_pass on the pass it opens.",
      }),
    ),
    reason: Type.Optional(
      Type.String({
        description:
          'Why the pass ran: "chat_only" | "empty_response" | "correction" for retry_pass.',
      }),
    ),
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
  data: {
    code: AiRunWarningCode | (string & {});
    message: string;
    pass?: number;
    reason?: string;
  };
};

/**
 * Token accounting for ONE provider call. A run makes several (every tool
 * round-trip is another call), so usage is reported per call rather than once
 * per run — summing is the consumer's job, and it cannot sum what it never saw.
 *
 * - `callId` — identifies the provider call these numbers belong to; stable
 *   across the events of that call, distinct between calls of the same run.
 * - `attempt` — 1-based retry counter within that call (a retried request bills
 *   again, so its usage is a separate event with the same `callId`).
 * - `step` — the run-loop iteration the call belongs to (0 when the emitter has
 *   no loop context; the run-loop re-stamps it as it passes through).
 * - `source` — `"stream"` when scraped off a streaming chunk, `"response"` when
 *   read from a non-streaming response body.
 * - `finalForCall` — true on the last usage event for this `callId`/`attempt`,
 *   so a consumer knows the numbers are settled and won't be superseded.
 *
 * Token fields are optional throughout: plenty of OpenAI-compatible servers
 * report partial usage, or none at all.
 */
export const AiRunUsageEventSchema = Type.Object({
  type: Type.Literal("run.usage"),
  ...runEventBaseFields,
  data: Type.Object({
    callId: Type.String(),
    attempt: Type.Number(),
    step: Type.Number(),
    model: Type.String(),
    promptTokens: Type.Optional(Type.Number()),
    completionTokens: Type.Optional(Type.Number()),
    totalTokens: Type.Optional(Type.Number()),
    source: Type.Union([Type.Literal("stream"), Type.Literal("response")]),
    finalForCall: Type.Boolean(),
  }),
});
export type AiRunUsageEvent = Static<typeof AiRunUsageEventSchema>;

/**
 * An agent-session turn compacted its older history into a derived summary
 * (ADR 0017 part 2) before the pass ran. Host-emitted; additive.
 *
 * - `summaryId` — the saved `ContextSummaryRecord`. Canonical history is
 *   untouched; the summary only stands in for the covered prefix.
 * - `tokensBefore` / `tokensAfter` — estimated size of the covered messages and
 *   of the summary message that replaced them.
 * - `coveredMessages` — how many canonical messages the summary covers.
 * - `reason` — `"threshold"` when the history went over budget, `"manual"` when
 *   `AgentSessionService.compact` asked for it.
 */
export const AiRunContextCompactedEventSchema = Type.Object({
  type: Type.Literal("run.context.compacted"),
  ...runEventBaseFields,
  data: Type.Object({
    summaryId: Type.String(),
    tokensBefore: Type.Number(),
    tokensAfter: Type.Number(),
    coveredMessages: Type.Number(),
    reason: Type.Union([Type.Literal("threshold"), Type.Literal("manual")]),
  }),
});
export type AiRunContextCompactedEvent = Static<
  typeof AiRunContextCompactedEventSchema
>;

/**
 * The outcome of ONE verification of this run — "did the work actually land?"
 * as answered after a pass, not as narrated by the model.
 *
 * Emitted only by a host that wired a verification hook AND opted into the
 * correction harness; a single-shot verification stays silent on the log, so a
 * consumer that never sees this event cannot conclude a run was unverified.
 *
 * - `pass` — 0 for the verification of the run's own answer, then 1, 2, … for
 *   each correction pass that followed. It is the harness's iteration counter,
 *   not an index into anything durable.
 * - `status` — `"pass"`/`"partial"` come straight from the hook's
 *   `DeficiencyReport`. `"unavailable"` is the framework's own: the hook threw
 *   or answered `null` mid-harness. It is NOT a pass — the harness stops
 *   rather than assume the work landed, which is the whole point of saying so
 *   on the log instead of omitting the event.
 * - `deficiencies` — the report's lines verbatim; empty for `pass` and for
 *   `unavailable`.
 */
export const AiRunVerificationEventSchema = Type.Object({
  type: Type.Literal("run.verification"),
  ...runEventBaseFields,
  data: Type.Object({
    pass: Type.Number({
      description:
        "0 for the run's own answer, then 1, 2, … per correction pass.",
    }),
    status: Type.Union([
      Type.Literal("pass"),
      Type.Literal("partial"),
      Type.Literal("unavailable"),
    ]),
    deficiencies: Type.Array(Type.String()),
  }),
});
export type AiRunVerificationEvent = Static<
  typeof AiRunVerificationEventSchema
>;

/**
 * A durable input (a steer) was placed into the run before a provider step
 * (ADR 0016 §3). Emitted only by a host that uses agent sessions.
 */
export const AiRunInputAppliedEventSchema = Type.Object({
  type: Type.Literal("run.input.applied"),
  ...runEventBaseFields,
  data: Type.Object({
    inputId: Type.String({
      description: "The durable input's idempotency key (`AgentInputStore`).",
    }),
    kind: Type.String({
      description: 'Open vocabulary; today only "steer".',
    }),
    step: Type.Number({
      description: "The 1-based provider step the input was placed before.",
    }),
  }),
});
export type AiRunInputAppliedEvent = Static<
  typeof AiRunInputAppliedEventSchema
>;

/**
 * The run paused for human input; the task parks in `waiting_approval` and
 * continues on a later attempt. NOT a terminal event (ADR 0016 §5). Emitted
 * only by a host that uses agent sessions.
 */
export const AiRunWaitingEventSchema = Type.Object({
  type: Type.Literal("run.waiting"),
  ...runEventBaseFields,
  data: Type.Object({
    requestId: Type.String({
      description:
        "Deterministic human-request id, `hreq:<taskId>:<toolCallId>`.",
    }),
    toolCallId: Type.String({
      description: "The tool call that asked for input.",
    }),
    toolName: Type.String({ description: "The tool that asked for input." }),
    step: Type.Number({ description: "The 1-based provider step." }),
    prompt: Type.String({ description: "What the human is asked." }),
    expiresAt: Type.Optional(
      Type.String({ description: "ISO-8601 expiry of the request." }),
    ),
    kind: Type.Optional(
      Type.Union([Type.Literal("input"), Type.Literal("approval")], {
        description:
          "`approval`: a human must approve this exact call (ADR 0019). Absent means `input`.",
      }),
    ),
    argsSha256: Type.Optional(
      Type.String({
        description:
          "Approval only: sha-256 hex of the call's canonical JSON arguments.",
      }),
    ),
    requestedMode: Type.Optional(
      Type.String({
        description:
          "Approval only: the privilege the call asked for, recorded so the human sees what was requested. Recording it grants nothing; the host decides what an approval means.",
      }),
    ),
  }),
});
export type AiRunWaitingEvent = Static<typeof AiRunWaitingEventSchema>;

export const AiRunCompletedEventSchema = Type.Object({
  type: Type.Literal("run.completed"),
  ...runEventBaseFields,
  data: Type.Object({
    iterations: Type.Number(),
    /**
     * Why the run stopped: the last turn's provider `finish_reason`, or one of
     * the loop's own — `"max_iterations"` (budget spent), `"incomplete"` (the
     * stream was cut before the provider said why; never normalized to
     * `"stop"`, which would claim an answer that never finished).
     */
    finishReason: Type.Optional(Type.String()),
  }),
});
export type AiRunCompletedEvent = Static<typeof AiRunCompletedEventSchema>;

export const AiRunFailedEventSchema = Type.Object({
  type: Type.Literal("run.failed"),
  ...runEventBaseFields,
  data: Type.Object({
    errorMessage: Type.String(),
    errorCode: Type.Optional(Type.String()),
  }),
});
export type AiRunFailedEvent = Static<typeof AiRunFailedEventSchema>;

export const AiRunCancelledEventSchema = Type.Object({
  type: Type.Literal("run.cancelled"),
  ...runEventBaseFields,
  data: Type.Object({ reason: Type.Optional(Type.String()) }),
});
export type AiRunCancelledEvent = Static<typeof AiRunCancelledEventSchema>;

export const AiRunEventSchema = Type.Union([
  AiRunStartedEventSchema,
  AiRunMessageDeltaEventSchema,
  AiRunMessageReasoningDeltaEventSchema,
  AiRunMessageCompletedEventSchema,
  AiRunToolRequestedEventSchema,
  AiRunToolRunningEventSchema,
  AiRunToolSucceededEventSchema,
  AiRunToolFailedEventSchema,
  AiRunWarningEventSchema,
  AiRunUsageEventSchema,
  AiRunContextCompactedEventSchema,
  AiRunVerificationEventSchema,
  AiRunInputAppliedEventSchema,
  AiRunWaitingEventSchema,
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
  | AiRunMessageReasoningDeltaEvent
  | AiRunMessageCompletedEvent
  | AiRunToolRequestedEvent
  | AiRunToolRunningEvent
  | AiRunToolSucceededEvent
  | AiRunToolFailedEvent
  | AiRunWarningEvent
  | AiRunUsageEvent
  | AiRunContextCompactedEvent
  | AiRunVerificationEvent
  | AiRunInputAppliedEvent
  | AiRunWaitingEvent
  | AiRunCompletedEvent
  | AiRunFailedEvent
  | AiRunCancelledEvent;
