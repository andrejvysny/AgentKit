import type { AiSourceRef } from "../sources/source-ref.js";
import type { AiToolCall } from "../tools/types.js";

export type AiRunEventType =
  | "run.started"
  | "run.message.delta"
  | "run.message.completed"
  | "run.tool.requested"
  | "run.tool.running"
  | "run.tool.succeeded"
  | "run.tool.failed"
  | "run.warning"
  | "run.completed"
  | "run.failed"
  | "run.cancelled";

export interface AiRunEventBase {
  type: AiRunEventType;
  runId: string;
  timestamp: string;
}

export interface AiRunStartedEvent extends AiRunEventBase {
  type: "run.started";
  data: { model: string; toolCount: number };
}

export interface AiRunMessageDeltaEvent extends AiRunEventBase {
  type: "run.message.delta";
  data: { delta: string };
}

export interface AiRunMessageCompletedEvent extends AiRunEventBase {
  type: "run.message.completed";
  data: {
    content: string;
    toolCallCount: number;
    toolCalls?: AiToolCall[];
    /** Chain-of-thought from reasoning models (OpenAI `reasoning_content`). */
    reasoningContent?: string;
    /** Raw provider finish_reason for this turn (e.g. "stop", "length", "tool_calls"). */
    finishReason?: string;
  };
}

export interface AiRunToolRequestedEvent extends AiRunEventBase {
  type: "run.tool.requested";
  data: {
    toolCallId: string;
    toolName: string;
    argumentsJson: string;
  };
}

export interface AiRunToolRunningEvent extends AiRunEventBase {
  type: "run.tool.running";
  data: { toolCallId: string; toolName: string };
}

export interface AiRunToolSucceededEvent extends AiRunEventBase {
  type: "run.tool.succeeded";
  data: {
    toolCallId: string;
    toolName: string;
    resultJson: string;
    sources: AiSourceRef[];
    truncated: boolean;
    warnings: string[];
    /** Tool-reported status (Track A/D). Distinguishes a partial apply from a clean success. */
    status?: "ok" | "partial";
    /** Short human/model-facing line summarising the result (Track A/D). */
    summary?: string;
    /** Slim payload fed to the model (Wave 0 §0.2). Full payload stays in `resultJson` for UI. */
    modelResultJson?: string;
  };
}

export interface AiRunToolFailedEvent extends AiRunEventBase {
  type: "run.tool.failed";
  data: {
    toolCallId: string;
    toolName: string;
    errorMessage: string;
    errorCode?: string;
    /**
     * #3: when a tool RAN and reported failure (esp. status:"partial"), the
     * balanced model envelope is carried here so consumers persist/replay it
     * faithfully instead of a generic error. Absent for pre-execution failures
     * (tool_missing/bad_args/schema_invalid/cap/cancelled) and exec throws.
     */
    modelResultJson?: string;
    status?: "error" | "partial";
  };
}

/**
 * Known `run.warning` codes. `code` stays an open string (the field type is `string`)
 * for forward-compat; this union documents the recognised values. Existing emitted code:
 * `tool_call_cap`. The P6 codes (`duplicate_loop` | `stall` | `tool_cap` | `timeout`) are
 * declared now but not emitted this iteration.
 */
export type AiRunWarningCode =
  | "tool_call_cap"
  | "duplicate_loop"
  | "stall"
  | "tool_cap"
  | "timeout";

export interface AiRunWarningEvent extends AiRunEventBase {
  type: "run.warning";
  data: { code: AiRunWarningCode | (string & {}); message: string };
}

export interface AiRunCompletedEvent extends AiRunEventBase {
  type: "run.completed";
  data: { iterations: number; finishReason?: string };
}

export interface AiRunFailedEvent extends AiRunEventBase {
  type: "run.failed";
  data: { errorMessage: string; errorCode?: string };
}

export interface AiRunCancelledEvent extends AiRunEventBase {
  type: "run.cancelled";
  data: { reason?: string };
}

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
