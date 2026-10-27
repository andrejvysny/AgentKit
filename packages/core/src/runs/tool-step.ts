import type {
  AiChatMessage,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import type { EventStamper } from "../events.js";
import { newToolEventId, nowIso } from "../ids.js";
import { truncateString } from "../tools/limits.js";
import {
  parseToolArguments,
  type ValidationError,
} from "../tools/validation.js";
import {
  buildEnvelope,
  envelopeTextBudget,
  errorEnvelope,
  safeStringify,
  serializeToolResult,
  type ToolFailureDetail,
} from "./tool-envelope.js";
import { executeToolSafely } from "./tool-exec.js";
import type { RunChatInput } from "./run-types.js";

export interface ToolBatchContext {
  input: RunChatInput;
  runId: string;
  stamp: EventStamper;
  /** The loop's private history; the batch appends the assistant + tool messages. */
  messages: AiChatMessage[];
  maxCallsPerIter: number;
  assistantContent: string;
  toolCalls: AiToolCall[];
  duplicateIds: string[];
  /** Mutated: synthesized announcements are added so none repeats. */
  announcedToolCallIds: Set<string>;
}

/**
 * Run one iteration's tool calls. Returns "cancelled" when the run was aborted
 * (its terminal event already yielded), else "continue" so the loop asks the
 * model again.
 */
export async function* executeToolBatch(
  ctx: ToolBatchContext,
): AsyncGenerator<AiRunEvent, "continue" | "cancelled", unknown> {
  const {
    input,
    runId,
    stamp,
    messages,
    maxCallsPerIter,
    assistantContent,
    duplicateIds,
    announcedToolCallIds,
  } = ctx;
  const toolCalls = ctx.toolCalls;
  if (duplicateIds.length > 0) {
    yield stamp({
      type: "run.warning",
      runId,
      timestamp: nowIso(),
      data: {
        code: "duplicate_tool_call_id",
        message: `Provider reused tool call id(s) ${duplicateIds.join(", ")} in one turn; the later call(s) were re-keyed so each stays answerable.`,
      },
    });
  }
  // A completed-only provider carries its tool calls solely in
  // run.message.completed.data.toolCalls and announces nothing, so a UI keyed
  // on run.tool.requested would show a tool jumping straight to running (or
  // never appearing at all). Synthesize the missing announcements — deduped
  // by id, so a provider that emits BOTH still produces exactly one per call —
  // before any of them executes.
  for (const tc of toolCalls) {
    if (announcedToolCallIds.has(tc.id)) continue;
    announcedToolCallIds.add(tc.id);
    yield stamp({
      type: "run.tool.requested",
      runId,
      timestamp: nowIso(),
      data: {
        toolCallId: tc.id,
        toolName: tc.name,
        argumentsJson: tc.argumentsJson,
      },
    });
  }

  const limitedToolCalls = toolCalls.slice(0, maxCallsPerIter);
  const skippedToolCalls = toolCalls.slice(maxCallsPerIter);
  if (skippedToolCalls.length > 0) {
    yield stamp({
      type: "run.warning",
      runId,
      timestamp: nowIso(),
      data: {
        code: "tool_call_cap",
        message: `Truncated ${toolCalls.length} tool calls to ${maxCallsPerIter} per iteration.`,
      },
    });
  }
  // The assistant message must list EVERY tool call it will answer (Chat
  // Completions rejects orphan tool_call_ids). Include ALL toolCalls here;
  // execute only the capped subset and emit a terminal tool message for each
  // skipped call below, so every tool_call_id has both an assistant entry and
  // a tool response.
  messages.push({
    role: "assistant",
    content: assistantContent,
    toolCalls: toolCalls,
  });

  // On cancellation mid-loop, every tool call already listed in the
  // assistant message (limited + skipped) must still receive a tool
  // response, or replayed history carries orphan tool_call_ids that the
  // provider rejects on the next turn. This emits a cancelled failure for
  // every call at/after `current` that hasn't been answered yet.
  const balanceCancelled = function* (
    current: AiToolCall,
  ): Generator<AiRunEvent, void, unknown> {
    const fromIdx = limitedToolCalls.indexOf(current);
    for (const rem of [...limitedToolCalls.slice(fromIdx), ...skippedToolCalls])
      yield* failTool(
        stamp,
        runId,
        messages,
        input.limits.maxBytes,
        rem,
        "Run cancelled before this tool produced a result.",
        "cancelled",
      );
  };

  for (const tc of limitedToolCalls) {
    // Abort race: stop before starting a tool if cancellation arrived.
    if (input.signal?.aborted) {
      yield* balanceCancelled(tc);
      yield stamp({
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
      return "cancelled";
    }
    yield stamp({
      type: "run.tool.running",
      runId,
      timestamp: nowIso(),
      data: { toolCallId: tc.id, toolName: tc.name },
    });
    const tool = input.registry.get(tc.name);
    if (!tool) {
      const err = `Tool not registered: ${tc.name}`;
      yield* failTool(
        stamp,
        runId,
        messages,
        input.limits.maxBytes,
        tc,
        err,
        "tool_missing",
      );
      continue;
    }
    const parsed = parseToolArguments(tc.argumentsJson);
    if (!parsed.ok) {
      const err = `Invalid arguments JSON: ${parsed.error}`;
      yield* failTool(
        stamp,
        runId,
        messages,
        input.limits.maxBytes,
        tc,
        err,
        "bad_args",
      );
      continue;
    }
    const args = parsed.value;

    // Validate against the tool's input schema BEFORE executing (Track B's Ajv
    // validator, precompiled at registration → full draft-07 coverage). On
    // errors, fail with an actionable message and feed it back; do not execute.
    const schemaErrors: ValidationError[] = input.registry.validateInput(
      tc.name,
      args,
    );
    if (schemaErrors.length > 0) {
      const err = describeValidationErrors(tc.name, schemaErrors);
      // `retryable: false` is about THIS call, not about the tool: the same
      // arguments will fail the same way, so a bare retry is pointless —
      // the model has to write different ones.
      yield* failTool(
        stamp,
        runId,
        messages,
        input.limits.maxBytes,
        tc,
        err,
        "schema_invalid",
        {
          phase: "validation",
          retryable: false,
        },
      );
      continue;
    }

    const result = await executeToolSafely(
      tool,
      {
        runId,
        chatId: input.chatId,
        scopeId: input.scopeId,
        userId: input.userId,
        bindings: input.bindings ?? [],
        limits: input.limits,
        signal: input.signal,
        metadata: { toolEventId: newToolEventId() },
      },
      args,
      input.defaultToolTimeoutMs,
    );

    // NOTE: do NOT abort here. This tool has already executed — its side
    // effects (e.g. an applied write) are real, so its result MUST be
    // emitted/persisted below; otherwise replayed history would say
    // "cancelled" while the design changed, causing a duplicate on retry. A
    // pending cancellation is caught at the next loop-top check (which
    // balances the remaining un-answered calls) and the outer-iteration
    // abort check — neither of which discards this completed result.

    // Branch on the tool's OWN ok, not just the executor wrapper: a tool that
    // returns { ok: false } is a failure even though it didn't throw.
    if (result.ok && result.value.ok) {
      const envelope = buildEnvelope(result.value, input.limits);
      // Serializing OUTSIDE the executor used to throw straight out of this
      // generator (a BigInt or a cycle in the result) — after the tool's
      // side effects were real, and with no terminal event for the run. The
      // tool ran; only the reporting failed, and that is what the model is
      // told.
      const serialized = serializeToolResult(envelope, result.value.data);
      if (!serialized.ok) {
        yield* failTool(
          stamp,
          runId,
          messages,
          input.limits.maxBytes,
          tc,
          `Tool ${tc.name} produced a result that could not be serialized: ${serialized.error}`,
          "result_unserializable",
          { phase: "execution", retryable: false },
        );
        continue;
      }
      const modelResultJson = serialized.envelopeJson;
      yield stamp({
        type: "run.tool.succeeded",
        runId,
        timestamp: nowIso(),
        data: {
          toolCallId: tc.id,
          toolName: tc.name,
          resultJson: serialized.dataJson,
          sources: result.value.sources,
          // The envelope's flag, not the tool's: it is true when the loop
          // itself had to cap an over-budget payload.
          truncated: envelope.truncated,
          warnings: result.value.warnings,
          status: envelope.status === "partial" ? "partial" : "ok",
          summary: result.value.summary,
          modelResultJson,
        },
      });
      // Feed the model the balanced envelope, not the raw data.
      messages.push({
        role: "tool",
        content: modelResultJson,
        toolCallId: tc.id,
        name: tc.name,
      });
    } else if (result.ok && !result.value.ok) {
      // Tool ran but reported failure: emit failed + feed an error envelope.
      const envelope = buildEnvelope(result.value, input.limits);
      const envelopeJson = safeStringify(envelope);
      if (!envelopeJson.ok) {
        yield* failTool(
          stamp,
          runId,
          messages,
          input.limits.maxBytes,
          tc,
          `Tool ${tc.name} produced a result that could not be serialized: ${envelopeJson.error}`,
          "result_unserializable",
          { phase: "execution", retryable: false },
        );
        continue;
      }
      // Capped like every other model-facing failure text: this comes
      // straight from the tool, and the envelope beside it is capped too.
      const errorMessage = truncateString(
        result.value.summary ??
          (result.value.warnings.length > 0
            ? result.value.warnings.join("; ")
            : `Tool ${tc.name} reported failure`),
        envelopeTextBudget(input.limits.maxBytes),
      ).value;
      yield stamp({
        type: "run.tool.failed",
        runId,
        timestamp: nowIso(),
        data: {
          toolCallId: tc.id,
          toolName: tc.name,
          errorMessage,
          errorCode: "tool_failed",
          // #3: carry the balanced envelope (preserves status:"partial" +
          // modelData) so run-service persists/replays it faithfully.
          status: envelope.status === "partial" ? "partial" : "error",
          modelResultJson: envelopeJson.json,
        },
      });
      messages.push({
        role: "tool",
        content: envelopeJson.json,
        toolCallId: tc.id,
        name: tc.name,
      });
    } else if (!result.ok) {
      yield* failTool(
        stamp,
        runId,
        messages,
        input.limits.maxBytes,
        tc,
        result.error,
        "exec_failed",
        { phase: "execution", retryable: result.retryable },
      );
    }
  }

  // Every capped/skipped call still needs a terminal event so no tool_call_id
  // is left dangling (and the message history stays balanced for the provider).
  for (const tc of skippedToolCalls) {
    const err = `Skipped: exceeded maxToolCallsPerIteration=${maxCallsPerIter}.`;
    yield stamp({
      type: "run.tool.failed",
      runId,
      timestamp: nowIso(),
      data: {
        toolCallId: tc.id,
        toolName: tc.name,
        errorMessage: err,
        errorCode: "tool_call_cap",
      },
    });
    messages.push({
      role: "tool",
      content: JSON.stringify(errorEnvelope("tool_call_cap", err)),
      toolCallId: tc.id,
      name: tc.name,
    });
  }
  // Loop again so the model can react to tool results.
  return "continue";
}

/**
 * Emit a run.tool.failed event and append the matching error tool message.
 *
 * `maxBytes` is the run's output budget: a thrown error's message is arbitrary
 * host text (a stack, a dumped row, a whole HTTP body) and it lands in
 * `messages`, so it is capped exactly like a successful result's prose is.
 */
function* failTool(
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
function describeValidationErrors(
  toolName: string,
  errors: ValidationError[],
): string {
  const detail = errors
    .map((e) => (e.path ? `${e.path}: ${e.message}` : e.message))
    .join("; ");
  return `Invalid arguments for ${toolName}: ${detail}. Fix the arguments and call again.`;
}
