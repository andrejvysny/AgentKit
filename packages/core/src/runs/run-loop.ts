import { newRunId, newToolEventId, nowIso } from "../ids.js";
import type { AiProviderClient } from "../providers/client.js";
import type { AiToolRegistry } from "../tools/registry.js";
import type { AiTool } from "../tools/tool.js";
import type {
  AiChatMessage,
  AiContextBinding,
  AiRunEvent,
  AiToolCall,
  AiToolEnvelope,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";
import {
  parseToolArguments,
  type ValidationError,
} from "../tools/validation.js";

export interface RunChatInput {
  client: AiProviderClient;
  registry: AiToolRegistry;
  model: string;
  /** Conversation so far. Treated as read-only: the loop works on a copy. */
  messages: readonly AiChatMessage[];
  bindings?: AiContextBinding[];
  limits: AiToolLimits;
  chatId?: string;
  userId?: string;
  temperature?: number;
  maxOutputTokens?: number;
  maxToolIterations?: number;
  maxToolCallsPerIteration?: number;
  signal?: AbortSignal;
  /** Override runId for deterministic tests. */
  runId?: string;
}

/**
 * What the run produced, returned as the generator's return value (`for await`
 * discards it — see the `collectRun` pattern in the tests to capture it).
 *
 * This is an in-process API type, deliberately NOT part of `@agentkit/contracts`:
 * nothing here crosses the wire. The events are the wire format; this is the
 * handoff between `runChat` and whoever drove it.
 */
export interface RunChatResult {
  runId: string;
  /** Which terminal event ended the run. */
  terminal: "completed" | "failed" | "cancelled";
  /**
   * Assistant + tool messages the run appended, in order. Append these to your
   * own history to continue the conversation.
   */
  appendedMessages: readonly AiChatMessage[];
  /** Provider round-trips actually taken (1-based). */
  iterations: number;
}

const DEFAULT_MAX_TOOL_ITERATIONS = 4;
const DEFAULT_MAX_TOOL_CALLS_PER_ITERATION = 8;

/**
 * Run a multi-turn chat with tool calls. Yields normalized AiRunEvents and
 * returns a {@link RunChatResult}.
 *
 * `input.messages` is never mutated: the loop appends to a private copy and hands
 * back what it appended. Mutating a caller's array was a hidden side effect that
 * made a run impossible to retry, fan out, or run twice from the same history —
 * the second attempt would start from a conversation the first one had rewritten.
 */
export async function* runChat(
  input: RunChatInput,
): AsyncGenerator<AiRunEvent, RunChatResult, unknown> {
  const runId = input.runId ?? newRunId();
  const maxIterations = input.maxToolIterations ?? DEFAULT_MAX_TOOL_ITERATIONS;
  const maxCallsPerIter =
    input.maxToolCallsPerIteration ?? DEFAULT_MAX_TOOL_CALLS_PER_ITERATION;
  const toolDefinitions = input.registry.listDefinitions();
  const messages: AiChatMessage[] = input.messages.slice();
  const initialLength = messages.length;
  let iteration = 0;
  let finishReason: string | undefined;

  /** Build the generator's return value for whichever terminal we reached. */
  const finish = (terminal: RunChatResult["terminal"]): RunChatResult => ({
    runId,
    terminal,
    appendedMessages: messages.slice(initialLength),
    iterations: iteration,
  });

  for (let i = 0; i < maxIterations; i++) {
    iteration = i + 1;
    if (input.signal?.aborted) {
      yield {
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      };
      return finish("cancelled");
    }

    // Deltas are display-only; the turn's authoritative content + tool calls come
    // from run.message.completed (so non-streaming providers that emit a single
    // completed event are handled). run.tool.requested events are a fallback for
    // providers/mocks that don't echo toolCalls into the completed payload.
    let deltaContent = "";
    let completedContent: string | undefined;
    let completedToolCalls: AiToolCall[] | undefined;
    const requestedToolCalls: AiToolCall[] = [];
    /** Tool call ids the provider already announced with run.tool.requested. */
    const announcedToolCallIds = new Set<string>();
    let turnFailed = false;
    let turnFinishReason: string | undefined;
    // #4: the real provider emits the "finish_reason=tool_calls, no usable call"
    // warning during streaming; track it so the final-block fallback doesn't
    // re-emit a duplicate (while still covering providers/mocks that don't).
    let sawNoToolCallWarning = false;

    // A provider client is expected to normalize its own transport failures into
    // run.failed, but a THROW still escapes — a bad JSON body, a socket reset, a
    // bug in a third-party client. Unwrapped it would blow past every consumer
    // as a raw rejection, leaving the run with no terminal event at all. Catch it
    // here and turn it into the one terminal event the contract promises.
    try {
      for await (const event of input.client.streamChat({
        runId,
        model: input.model,
        messages,
        tools: toolDefinitions.length > 0 ? toolDefinitions : undefined,
        temperature: input.temperature,
        maxOutputTokens: input.maxOutputTokens,
        signal: input.signal,
      })) {
        // Re-yield provider events with the canonical runId.
        const stamped = { ...event, runId } as AiRunEvent;
        switch (stamped.type) {
          case "run.message.delta":
            deltaContent += stamped.data.delta;
            yield stamped;
            break;
          case "run.message.completed":
            completedContent = stamped.data.content;
            if (stamped.data.toolCalls && stamped.data.toolCalls.length > 0) {
              completedToolCalls = stamped.data.toolCalls;
            }
            turnFinishReason = stamped.data.finishReason;
            yield stamped;
            break;
          case "run.tool.requested":
            requestedToolCalls.push({
              id: stamped.data.toolCallId,
              name: stamped.data.toolName,
              argumentsJson: stamped.data.argumentsJson,
            });
            announcedToolCallIds.add(stamped.data.toolCallId);
            yield stamped;
            break;
          case "run.failed":
            // The provider already emitted run.failed; record it and don't re-emit.
            turnFailed = true;
            yield stamped;
            break;
          case "run.cancelled":
            yield stamped;
            return finish("cancelled");
          case "run.started":
            if (iteration === 1) yield stamped;
            break;
          case "run.warning":
            if (stamped.data.code === "tool_call_unparseable")
              sawNoToolCallWarning = true;
            yield stamped;
            break;
          default:
            yield stamped;
        }
      }
    } catch (err) {
      // An abort surfaces as a throw from fetch/the stream reader. That is a
      // cancellation, not a provider fault — classifying it as failure would
      // make every user-cancelled run look broken.
      if (input.signal?.aborted || isAbortError(err)) {
        yield {
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: "aborted" },
        };
        return finish("cancelled");
      }
      yield {
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: {
          errorMessage: errorMessageOf(err),
          errorCode: "provider_error",
        },
      };
      return finish("failed");
    }

    if (turnFailed) {
      // Provider already yielded run.failed during the stream — don't double-emit.
      return finish("failed");
    }

    const assistantContent = completedContent ?? deltaContent;
    const turnToolCalls = completedToolCalls ?? requestedToolCalls;

    // If the assistant produced tool calls, append the assistant message with tool_calls
    // and execute each tool, appending role:'tool' messages with tool_call_id.
    if (turnToolCalls.length > 0) {
      // A completed-only provider carries its tool calls solely in
      // run.message.completed.data.toolCalls and announces nothing, so a UI keyed
      // on run.tool.requested would show a tool jumping straight to running (or
      // never appearing at all). Synthesize the missing announcements — deduped
      // by id, so a provider that emits BOTH still produces exactly one per call —
      // before any of them executes.
      for (const tc of turnToolCalls) {
        if (announcedToolCallIds.has(tc.id)) continue;
        announcedToolCallIds.add(tc.id);
        yield {
          type: "run.tool.requested",
          runId,
          timestamp: nowIso(),
          data: {
            toolCallId: tc.id,
            toolName: tc.name,
            argumentsJson: tc.argumentsJson,
          },
        };
      }

      const limitedToolCalls = turnToolCalls.slice(0, maxCallsPerIter);
      const skippedToolCalls = turnToolCalls.slice(maxCallsPerIter);
      if (skippedToolCalls.length > 0) {
        yield {
          type: "run.warning",
          runId,
          timestamp: nowIso(),
          data: {
            code: "tool_call_cap",
            message: `Truncated ${turnToolCalls.length} tool calls to ${maxCallsPerIter} per iteration.`,
          },
        };
      }
      // The assistant message must list EVERY tool call it will answer (Chat
      // Completions rejects orphan tool_call_ids). Include ALL turnToolCalls here;
      // execute only the capped subset and emit a terminal tool message for each
      // skipped call below, so every tool_call_id has both an assistant entry and
      // a tool response.
      messages.push({
        role: "assistant",
        content: assistantContent,
        toolCalls: turnToolCalls,
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
        for (const rem of [
          ...limitedToolCalls.slice(fromIdx),
          ...skippedToolCalls,
        ])
          yield* failTool(
            runId,
            messages,
            rem,
            "Run cancelled before this tool produced a result.",
            "cancelled",
          );
      };

      for (const tc of limitedToolCalls) {
        // Abort race: stop before starting a tool if cancellation arrived.
        if (input.signal?.aborted) {
          yield* balanceCancelled(tc);
          yield {
            type: "run.cancelled",
            runId,
            timestamp: nowIso(),
            data: { reason: "aborted" },
          };
          return finish("cancelled");
        }
        yield {
          type: "run.tool.running",
          runId,
          timestamp: nowIso(),
          data: { toolCallId: tc.id, toolName: tc.name },
        };
        const tool = input.registry.get(tc.name);
        if (!tool) {
          const err = `Tool not registered: ${tc.name}`;
          yield* failTool(runId, messages, tc, err, "tool_missing");
          continue;
        }
        const parsed = parseToolArguments(tc.argumentsJson);
        if (!parsed.ok) {
          const err = `Invalid arguments JSON: ${parsed.error}`;
          yield* failTool(runId, messages, tc, err, "bad_args");
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
          yield* failTool(runId, messages, tc, err, "schema_invalid");
          continue;
        }

        const result = await executeToolSafely(
          tool,
          {
            runId,
            chatId: input.chatId,
            userId: input.userId,
            bindings: input.bindings ?? [],
            limits: input.limits,
            signal: input.signal,
            metadata: { toolEventId: newToolEventId() },
          },
          args,
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
          const envelope = buildEnvelope(result.value);
          const modelResultJson = JSON.stringify(envelope);
          yield {
            type: "run.tool.succeeded",
            runId,
            timestamp: nowIso(),
            data: {
              toolCallId: tc.id,
              toolName: tc.name,
              resultJson: JSON.stringify(result.value.data),
              sources: result.value.sources,
              truncated: result.value.truncated,
              warnings: result.value.warnings,
              status: envelope.status === "partial" ? "partial" : "ok",
              summary: result.value.summary,
              modelResultJson,
            },
          };
          // Feed the model the balanced envelope, not the raw data.
          messages.push({
            role: "tool",
            content: modelResultJson,
            toolCallId: tc.id,
            name: tc.name,
          });
        } else if (result.ok && !result.value.ok) {
          // Tool ran but reported failure: emit failed + feed an error envelope.
          const envelope = buildEnvelope(result.value);
          const errorMessage =
            result.value.summary ??
            (result.value.warnings.length > 0
              ? result.value.warnings.join("; ")
              : `Tool ${tc.name} reported failure`);
          yield {
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
              modelResultJson: JSON.stringify(envelope),
            },
          };
          messages.push({
            role: "tool",
            content: JSON.stringify(envelope),
            toolCallId: tc.id,
            name: tc.name,
          });
        } else if (!result.ok) {
          yield* failTool(
            runId,
            messages,
            tc,
            result.error,
            "exec_failed",
          );
        }
      }

      // Every capped/skipped call still needs a terminal event so no tool_call_id
      // is left dangling (and the message history stays balanced for the provider).
      for (const tc of skippedToolCalls) {
        const err = `Skipped: exceeded maxToolCallsPerIteration=${maxCallsPerIter}.`;
        yield {
          type: "run.tool.failed",
          runId,
          timestamp: nowIso(),
          data: {
            toolCallId: tc.id,
            toolName: tc.name,
            errorMessage: err,
            errorCode: "tool_call_cap",
          },
        };
        messages.push({
          role: "tool",
          content: JSON.stringify(errorEnvelope("tool_call_cap", err)),
          toolCallId: tc.id,
          name: tc.name,
        });
      }
      // Loop again so the model can react to tool results.
      continue;
    }

    // No tool calls; commit final assistant message and finish.
    messages.push({ role: "assistant", content: assistantContent });
    finishReason = turnFinishReason ?? "stop";
    // #4: surface finish_reason="tool_calls" with no reconstructable call — but
    // only if the provider didn't already emit it during streaming (dedup).
    if (finishReason === "tool_calls" && !sawNoToolCallWarning) {
      yield {
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "tool_call_unparseable",
          message:
            "Provider reported finish_reason=tool_calls but emitted no usable tool call.",
        },
      };
    }
    if (finishReason === "length") {
      yield {
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "truncated",
          message:
            "Response truncated (finish_reason=length); increase max output tokens.",
        },
      };
    }
    yield {
      type: "run.completed",
      runId,
      timestamp: nowIso(),
      data: { iterations: iteration, finishReason },
    };
    return finish("completed");
  }

  yield {
    type: "run.warning",
    runId,
    timestamp: nowIso(),
    data: {
      code: "max_iterations",
      message: `Reached maxToolIterations=${maxIterations} without final answer.`,
    },
  };
  // Exhausting the iteration budget still ends on run.completed (with a warning),
  // so the terminal reported here matches the event stream.
  yield {
    type: "run.completed",
    runId,
    timestamp: nowIso(),
    data: { iterations: iteration, finishReason: "max_iterations" },
  };
  return finish("completed");
}

/** `err.message ?? err`, without assuming `err` is an Error at all. */
function errorMessageOf(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "object" && err !== null && "message" in err) {
    const message = (err as { message: unknown }).message;
    if (message !== undefined && message !== null) return String(message);
  }
  return String(err);
}

/** DOMException/AbortError duck-typing (works across realms and polyfills). */
function isAbortError(err: unknown): boolean {
  return (
    typeof err === "object" &&
    err !== null &&
    "name" in err &&
    (err as { name: unknown }).name === "AbortError"
  );
}

/** Emit a run.tool.failed event and append the matching error tool message. */
function* failTool(
  runId: string,
  messages: AiChatMessage[],
  tc: AiToolCall,
  errorMessage: string,
  errorCode: string,
): Generator<AiRunEvent, void, unknown> {
  yield {
    type: "run.tool.failed",
    runId,
    timestamp: nowIso(),
    data: { toolCallId: tc.id, toolName: tc.name, errorMessage, errorCode },
  };
  messages.push({
    role: "tool",
    content: JSON.stringify(errorEnvelope(errorCode, errorMessage)),
    toolCallId: tc.id,
    name: tc.name,
  });
}

/**
 * Build the Wave-0 balanced error envelope (§0.2) fed to the model on a failure
 * path (tool missing / bad JSON / schema_invalid / exec_failed / cap). The
 * structured `data.errorCode`/`errorMessage` lets the model react precisely.
 */
function errorEnvelope(
  errorCode: string,
  errorMessage: string,
): AiToolEnvelope {
  return {
    ok: false,
    status: "error",
    summary: errorMessage,
    warnings: [],
    truncated: false,
    data: { errorCode, errorMessage },
  };
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

/**
 * Build the balanced model-facing envelope (Wave 0 §0.2) from a tool result.
 * `data` carries `modelData` when present, else the full `data`. The full payload
 * stays in the success event's `resultJson` for UI/persistence.
 */
function buildEnvelope(result: AiToolResult<unknown>): AiToolEnvelope {
  // A tool that reports status:"partial" keeps "partial" even when ok:false
  // (a partial apply is not a hard error — F5b). Only ok:false WITHOUT a partial
  // status maps to "error".
  const status: AiToolEnvelope["status"] =
    result.status === "partial" ? "partial" : result.ok ? "ok" : "error";
  return {
    ok: result.ok,
    status,
    summary: result.summary,
    warnings: result.warnings,
    truncated: result.truncated,
    data: result.modelData ?? result.data,
  };
}

async function executeToolSafely(
  tool: AiTool<unknown, unknown>,
  ctx: Parameters<AiTool["execute"]>[0],
  input: unknown,
): Promise<
  { ok: true; value: AiToolResult<unknown> } | { ok: false; error: string }
> {
  const timeoutMs = tool.definition.timeoutMs;
  // No per-tool timeout: unchanged fast path (every local tool hits this).
  if (!timeoutMs || timeoutMs <= 0) {
    try {
      const value = await tool.execute(ctx, input);
      return { ok: true, value };
    } catch (err) {
      return {
        ok: false,
        error: err instanceof Error ? err.message : String(err),
      };
    }
  }
  // Per-tool timeout: abort via a controller linked to the run-level signal, so
  // either the run cancelling OR the deadline elapsing aborts the tool's fetch.
  const controller = new AbortController();
  const parent = ctx.signal;
  const onParentAbort = () => controller.abort(parent?.reason);
  if (parent) {
    if (parent.aborted) controller.abort(parent.reason);
    else parent.addEventListener("abort", onParentAbort, { once: true });
  }
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);
  try {
    const value = await tool.execute(
      { ...ctx, signal: controller.signal },
      input,
    );
    return { ok: true, value };
  } catch (err) {
    if (timedOut) {
      return {
        ok: false,
        error: `Tool ${tool.definition.name} timed out after ${timeoutMs}ms`,
      };
    }
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
    };
  } finally {
    clearTimeout(timer);
    if (parent) parent.removeEventListener("abort", onParentAbort);
  }
}
