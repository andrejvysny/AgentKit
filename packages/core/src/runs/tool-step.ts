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
} from "./tool-envelope.js";
import { executeToolSafely } from "./tool-exec.js";
import type { AgentLoopInput } from "./agent-loop.js";
import { describeValidationErrors, failTool } from "./tool-failure.js";
import {
  humanRequestId,
  SUSPEND_CONFLICT_CODE,
  SUSPEND_INVALID_CODE,
  type AgentSuspendRequest,
} from "./session-seams.js";
import {
  frozenClone,
  hasHooks,
  runObservers,
  runToolGate,
  TOOL_HOOK_DENIED_CODE,
} from "./loop-hooks.js";

export interface ToolBatchContext {
  input: AgentLoopInput;
  runId: string;
  /** 1-based loop iteration, handed to hooks. */
  step: number;
  stamp: EventStamper;
  /** The loop's private history; the batch appends the assistant + tool messages. */
  messages: AiChatMessage[];
  maxCallsPerIter: number;
  assistantContent: string;
  toolCalls: AiToolCall[];
  duplicateIds: string[];
  /** Mutated: synthesized announcements are added so none repeats. */
  announcedToolCallIds: Set<string>;
  /**
   * The calls' assistant message is already in `messages` (an approved call
   * resumed after a park): push no assistant message and announce nothing.
   */
  assistantInHistory?: boolean;
}

/**
 * Run one iteration's tool calls. Returns "cancelled" when the run was aborted
 * (its terminal event already yielded), "suspended" when a tool asked for human
 * input (`run.waiting` yielded, deliberately no terminal event), else
 * "continue" so the loop asks the model again.
 */
export async function* executeToolBatch(
  ctx: ToolBatchContext,
): AsyncGenerator<AiRunEvent, "continue" | "cancelled" | "suspended", unknown> {
  const {
    input,
    runId,
    step,
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
  for (const tc of ctx.assistantInHistory ? [] : toolCalls) {
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

  const hooked = hasHooks(input);
  const toolResult = (
    tc: AiToolCall,
    status: "succeeded" | "failed",
    errorCode?: string,
  ) =>
    runObservers(input, runId, stamp, "onToolResult", {
      runId,
      step,
      toolCallId: tc.id,
      toolName: tc.name,
      status,
      errorCode,
      signal: input.signal,
    });

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
  if (!ctx.assistantInHistory)
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

  // The one call (first wins) that is waiting for a human. It gets no tool
  // message now: its result is the human's later answer.
  let suspension:
    | {
        tc: AiToolCall;
        prompt: string;
        expiresAt?: string;
        kind?: "input" | "approval";
        argsSha256?: string;
        approvalGeneration?: string;
        contextSha256?: string;
        requestedMode?: string;
        standing?: { digest: string; label: string };
      }
    | undefined;
  // Cancellation must still answer the suspended call, or its tool_call_id
  // stays orphaned in the replayed history.
  const answerSuspended = function* (): Generator<AiRunEvent, void, unknown> {
    if (!suspension) return;
    const { tc } = suspension;
    suspension = undefined;
    yield* failTool(
      stamp,
      runId,
      messages,
      input.limits.maxBytes,
      tc,
      "Run cancelled before this tool produced a result.",
      "cancelled",
    );
  };

  for (const tc of limitedToolCalls) {
    // Abort race: stop before starting a tool if cancellation arrived.
    if (input.signal?.aborted) {
      yield* answerSuspended();
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
      if (hooked) yield* toolResult(tc, "failed", "tool_missing");
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
      if (hooked) yield* toolResult(tc, "failed", "bad_args");
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
      if (hooked) yield* toolResult(tc, "failed", "schema_invalid");
      continue;
    }

    if (hooked) {
      const gate = await runToolGate(input, {
        runId,
        step,
        toolCallId: tc.id,
        toolName: tc.name,
        args: frozenClone(args),
        signal: input.signal,
      });
      if (!gate.allowed) {
        yield* failTool(
          stamp,
          runId,
          messages,
          input.limits.maxBytes,
          tc,
          gate.message,
          TOOL_HOOK_DENIED_CODE,
          { phase: "guard", retryable: false },
        );
        yield* toolResult(tc, "failed", TOOL_HOOK_DENIED_CODE);
        continue;
      }
      // Same rule as the loop-top check: nothing dispatches after cancel.
      if (input.signal?.aborted) {
        yield* answerSuspended();
        yield* balanceCancelled(tc);
        yield stamp({
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: "aborted" },
        });
        return "cancelled";
      }
    }

    // Per-call holder: the closure is sealed once execute settles, so a tool
    // that fires `suspend` late (after a timeout) cannot park a finished call.
    let suspendRequest: AgentSuspendRequest | undefined;
    let sealed = false;
    const suspend = input.suspendable
      ? (request: AgentSuspendRequest): void => {
          if (sealed || suspendRequest) return;
          suspendRequest = {
            prompt: request?.prompt,
            ...(request?.expiresAt ? { expiresAt: request.expiresAt } : {}),
            ...(request?.kind ? { kind: request.kind } : {}),
            ...(request?.argsSha256 ? { argsSha256: request.argsSha256 } : {}),
            ...(request?.approvalGeneration
              ? { approvalGeneration: request.approvalGeneration }
              : {}),
            ...(request?.contextSha256
              ? { contextSha256: request.contextSha256 }
              : {}),
            ...(request?.requestedMode
              ? { requestedMode: request.requestedMode }
              : {}),
            ...(request?.standing ? { standing: request.standing } : {}),
          };
        }
      : undefined;
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
        toolCallId: tc.id,
        metadata: { ...input.toolMetadata, toolEventId: newToolEventId() },
        // Key absent (not undefined) when not suspendable.
        ...(suspend ? { suspend } : {}),
      },
      args,
      input.defaultToolTimeoutMs,
    );
    sealed = true;

    if (suspendRequest) {
      const {
        prompt,
        expiresAt,
        kind,
        argsSha256,
        approvalGeneration,
        contextSha256,
        requestedMode,
        standing,
      } = suspendRequest as AgentSuspendRequest;
      const fail = (message: string, code: string) =>
        failTool(
          stamp,
          runId,
          messages,
          input.limits.maxBytes,
          tc,
          message,
          code,
          {
            phase: "execution",
            retryable: false,
          },
        );
      if (typeof prompt !== "string" || prompt.trim() === "") {
        yield* fail(
          "suspend() needs a non-empty prompt for the human.",
          SUSPEND_INVALID_CODE,
        );
        if (hooked) yield* toolResult(tc, "failed", SUSPEND_INVALID_CODE);
      } else if (suspension) {
        yield* fail(
          "Another tool call in this step is already waiting for human input.",
          SUSPEND_CONFLICT_CODE,
        );
        if (hooked) yield* toolResult(tc, "failed", SUSPEND_CONFLICT_CODE);
      } else {
        suspension = {
          tc,
          prompt,
          ...(expiresAt ? { expiresAt } : {}),
          ...(kind ? { kind } : {}),
          ...(argsSha256 ? { argsSha256 } : {}),
          ...(approvalGeneration ? { approvalGeneration } : {}),
          ...(contextSha256 ? { contextSha256 } : {}),
          ...(requestedMode ? { requestedMode } : {}),
          ...(standing ? { standing } : {}),
        };
      }
      continue;
    }

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
        if (hooked) yield* toolResult(tc, "failed", "result_unserializable");
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
      if (hooked) yield* toolResult(tc, "succeeded");
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
        if (hooked) yield* toolResult(tc, "failed", "result_unserializable");
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
      if (hooked) yield* toolResult(tc, "failed", "tool_failed");
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
      if (hooked) yield* toolResult(tc, "failed", "exec_failed");
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
    if (hooked) yield* toolResult(tc, "failed", "tool_call_cap");
  }
  if (suspension) {
    // Cancellation wins: the abort is caught here rather than at the next
    // loop-top, because "suspended" would otherwise park an aborted run.
    if (input.signal?.aborted) {
      yield* answerSuspended();
      yield stamp({
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
      return "cancelled";
    }
    yield stamp({
      type: "run.waiting",
      runId,
      timestamp: nowIso(),
      data: {
        requestId: humanRequestId(
          runId,
          suspension.tc.id,
          suspension.approvalGeneration,
        ),
        toolCallId: suspension.tc.id,
        toolName: suspension.tc.name,
        step,
        prompt: suspension.prompt,
        ...(suspension.expiresAt ? { expiresAt: suspension.expiresAt } : {}),
        ...(suspension.kind ? { kind: suspension.kind } : {}),
        ...(suspension.argsSha256 ? { argsSha256: suspension.argsSha256 } : {}),
        ...(suspension.approvalGeneration
          ? { approvalGeneration: suspension.approvalGeneration }
          : {}),
        ...(suspension.contextSha256
          ? { contextSha256: suspension.contextSha256 }
          : {}),
        ...(suspension.requestedMode
          ? { requestedMode: suspension.requestedMode }
          : {}),
        ...(suspension.standing ? { standing: suspension.standing } : {}),
      },
    });
    return "suspended";
  }
  // Loop again so the model can react to tool results.
  return "continue";
}
