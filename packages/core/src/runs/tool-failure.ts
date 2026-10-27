import type {
  AiChatMessage,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import type { EventStamper } from "../events.js";
import { nowIso } from "../ids.js";
import { truncateString } from "../tools/limits.js";
import type { ValidationError } from "../tools/validation.js";
import {
  envelopeTextBudget,
  errorEnvelope,
  type ToolFailureDetail,
} from "./tool-envelope.js";

/**
 * Emit a run.tool.failed event and append the matching error tool message.
 *
 * `maxBytes` is the run's output budget: a thrown error's message is arbitrary
 * host text (a stack, a dumped row, a whole HTTP body) and it lands in
 * `messages`, so it is capped exactly like a successful result's prose is.
 */
export function* failTool(
  stamp: EventStamper,
  runId: string,
  messages: AiChatMessage[],
  maxBytes: number,
  tc: AiToolCall,
  rawErrorMessage: string,
  errorCode: string,
  detail: ToolFailureDetail = {},
): Generator<AiRunEvent, void, unknown> {
  const errorMessage = truncateString(
    rawErrorMessage,
    envelopeTextBudget(maxBytes),
  ).value;
  yield stamp({
    type: "run.tool.failed",
    runId,
    timestamp: nowIso(),
    data: { toolCallId: tc.id, toolName: tc.name, errorMessage, errorCode },
  });
  messages.push({
    role: "tool",
    content: JSON.stringify(errorEnvelope(errorCode, errorMessage, detail)),
    toolCallId: tc.id,
    name: tc.name,
  });
}

/** Turn schema validation errors into an actionable, model-facing message. */
export function describeValidationErrors(
  toolName: string,
  errors: ValidationError[],
): string {
  const detail = errors
    .map((e) => (e.path ? `${e.path}: ${e.message}` : e.message))
    .join("; ");
  return `Invalid arguments for ${toolName}: ${detail}. Fix the arguments and call again.`;
}
