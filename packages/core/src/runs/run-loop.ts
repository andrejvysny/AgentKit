import { newRunId, newToolEventId, nowIso } from "../ids.js";
import { createEventStamper, type EventStamper } from "../events.js";
import type { AiProviderClient } from "../providers/client.js";
import type { AiToolRegistry } from "../tools/registry.js";
import type { AiTool } from "../tools/tool.js";
import type {
  AiChatMessage,
  AiContextBinding,
  AiRunEvent,
  AiToolCall,
  AiToolEnvelope,
  AiToolErrorData,
  AiToolErrorPhase,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";
import {
  parseToolArguments,
  type ValidationError,
} from "../tools/validation.js";
import { truncateString } from "../tools/limits.js";

export interface RunChatInput {
  client: AiProviderClient;
  registry: AiToolRegistry;
  model: string;
  /** Conversation so far. Treated as read-only: the loop works on a copy. */
  messages: readonly AiChatMessage[];
  bindings?: AiContextBinding[];
  limits: AiToolLimits;
  chatId?: string;
  /**
   * The serialization/idempotency scope this run writes to, passed through
   * verbatim to every tool's {@link AiToolExecutionContext}. The loop does not
   * interpret it: only the host knows whether a run is scoped on its chat or on
   * the document several chats share.
   */
  scopeId?: string;
  userId?: string;
  temperature?: number;
  maxOutputTokens?: number;
  maxToolIterations?: number;
  maxToolCallsPerIteration?: number;
  /**
   * Deadline applied to any tool whose definition sets no `timeoutMs` of its
   * own. Undefined (the default) keeps the historical behaviour: such tools run
   * unbounded. A host that cannot afford a hung local tool to hold its lease
   * forever sets this once for the whole run.
   */
  defaultToolTimeoutMs?: number;
  signal?: AbortSignal;
  /** Override runId for deterministic tests. */
  runId?: string;
  /**
   * First `seq` stamped on this run's events. Resume a stream without breaking
   * the consumer's ordering key by passing the number after the last one already
   * delivered. Default 0.
   */
  firstSeq?: number;
  /** Stamped on every event; groups the events of one attempt of this run. */
  attemptId?: string;
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
  // Clamped: `0` (or a negative) used to mean "never call the provider" and end
  // the run as a silent empty success, which reads as a model that answered
  // nothing rather than as the misconfiguration it is.
  const maxIterations = Math.max(
    1,
    input.maxToolIterations ?? DEFAULT_MAX_TOOL_ITERATIONS,
  );
  const maxCallsPerIter =
    input.maxToolCallsPerIteration ?? DEFAULT_MAX_TOOL_CALLS_PER_ITERATION;
  const toolDefinitions = input.registry.listDefinitions();
  const messages: AiChatMessage[] = input.messages.slice();
  const initialLength = messages.length;
  // EVERY event leaves through this stamper — loop-originated and re-yielded
  // provider events alike — so one run emits one unbroken, gap-detectable
  // sequence. A client's own numbering is deliberately overwritten: it counts
  // per call, and a run spans several calls from possibly several clients.
  const stamp = createEventStamper({
    firstSeq: input.firstSeq,
    attemptId: input.attemptId,
  });
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
      yield stamp({
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
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
            yield stamp(stamped);
            break;
          case "run.message.completed":
            completedContent = stamped.data.content;
            if (stamped.data.toolCalls && stamped.data.toolCalls.length > 0) {
              completedToolCalls = stamped.data.toolCalls;
            }
            turnFinishReason = stamped.data.finishReason;
            yield stamp(stamped);
            break;
          case "run.tool.requested":
            requestedToolCalls.push({
              id: stamped.data.toolCallId,
              name: stamped.data.toolName,
              argumentsJson: stamped.data.argumentsJson,
            });
            announcedToolCallIds.add(stamped.data.toolCallId);
            yield stamp(stamped);
            break;
          case "run.failed":
            // The provider already emitted run.failed; record it and don't re-emit.
            turnFailed = true;
            yield stamp(stamped);
            break;
          case "run.cancelled":
            yield stamp(stamped);
            return finish("cancelled");
          case "run.started":
            if (iteration === 1) yield stamp(stamped);
            break;
          case "run.warning":
            if (stamped.data.code === "tool_call_unparseable")
              sawNoToolCallWarning = true;
            // The flatten is recomputed identically on every call — same
            // messages in, same parts dropped — so iterations 2..n re-announce
            // exactly what iteration 1 already said. One durable warning per
            // run is the signal a consumer acts on; N copies of it are noise
            // that consumer then has to dedupe. Suppressed the same way
            // `run.started` is, and for the same reason.
            if (stamped.data.code === "multimodal_flattened" && iteration > 1)
              break;
            yield stamp(stamped);
            break;
          case "run.usage":
            // The client counts tokens but has no idea which round-trip it is;
            // only the loop knows. Stamp the iteration so a consumer can
            // attribute spend to a step without tracking call order itself.
            yield stamp({
              ...stamped,
              data: { ...stamped.data, step: iteration },
            });
            break;
          default:
            yield stamp(stamped);
        }
      }
    } catch (err) {
      // An abort surfaces as a throw from fetch/the stream reader. That is a
      // cancellation, not a provider fault — classifying it as failure would
      // make every user-cancelled run look broken.
      if (input.signal?.aborted || isAbortError(err)) {
        yield stamp({
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: "aborted" },
        });
        return finish("cancelled");
      }
      yield stamp({
        type: "run.failed",
        runId,
        timestamp: nowIso(),
        data: {
          errorMessage: errorMessageOf(err),
          errorCode: "provider_error",
        },
      });
      return finish("failed");
    }

    if (turnFailed) {
      // Provider already yielded run.failed during the stream — don't double-emit.
      return finish("failed");
    }

    const assistantContent = completedContent ?? deltaContent;
    const turnToolCalls = completedToolCalls ?? requestedToolCalls;

    // Hoisted above the tool-call branch on purpose: a turn cut off at the
    // token budget usually breaks the TOOL ARGUMENTS (which then fail as
    // bad_args with no hint why), and that is exactly the case the old
    // no-tool-calls-only placement never warned about.
    if (turnFinishReason === "length") {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "truncated",
          message:
            "Response truncated (finish_reason=length); increase max output tokens.",
        },
      });
    }

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

      const limitedToolCalls = turnToolCalls.slice(0, maxCallsPerIter);
      const skippedToolCalls = turnToolCalls.slice(maxCallsPerIter);
      if (skippedToolCalls.length > 0) {
        yield stamp({
          type: "run.warning",
          runId,
          timestamp: nowIso(),
          data: {
            code: "tool_call_cap",
            message: `Truncated ${turnToolCalls.length} tool calls to ${maxCallsPerIter} per iteration.`,
          },
        });
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
            stamp,
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
          yield stamp({
            type: "run.cancelled",
            runId,
            timestamp: nowIso(),
            data: { reason: "aborted" },
          });
          return finish("cancelled");
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
          yield* failTool(stamp, runId, messages, tc, err, "tool_missing");
          continue;
        }
        const parsed = parseToolArguments(tc.argumentsJson);
        if (!parsed.ok) {
          const err = `Invalid arguments JSON: ${parsed.error}`;
          yield* failTool(stamp, runId, messages, tc, err, "bad_args");
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
          yield* failTool(stamp, runId, messages, tc, err, "schema_invalid", {
            phase: "validation",
            retryable: false,
          });
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
              tc,
              `Tool ${tc.name} produced a result that could not be serialized: ${envelopeJson.error}`,
              "result_unserializable",
              { phase: "execution", retryable: false },
            );
            continue;
          }
          const errorMessage =
            result.value.summary ??
            (result.value.warnings.length > 0
              ? result.value.warnings.join("; ")
              : `Tool ${tc.name} reported failure`);
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
      continue;
    }

    // No tool calls; commit final assistant message and finish.
    // `turnFinishReason` may be the synthetic "incomplete" (stream cut before
    // the provider said why) — passed through untouched, because defaulting it
    // to "stop" is precisely the lie that made a half answer look final.
    finishReason = turnFinishReason ?? "stop";
    // #4: surface finish_reason="tool_calls" with no reconstructable call — but
    // only if the provider didn't already emit it during streaming (dedup).
    if (finishReason === "tool_calls" && !sawNoToolCallWarning) {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "tool_call_unparseable",
          message:
            "Provider reported finish_reason=tool_calls but emitted no usable tool call.",
        },
      });
    }
    // Cancellation arriving while the last chunk was in flight must not be
    // committed as a finished answer. Checked HERE, immediately before the
    // commit and its terminal event: every yield above handed control back to
    // the consumer, so the signal can have moved since the loop-top check.
    if (input.signal?.aborted) {
      yield stamp({
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
      return finish("cancelled");
    }
    messages.push({ role: "assistant", content: assistantContent });
    yield stamp({
      type: "run.completed",
      runId,
      timestamp: nowIso(),
      data: { iterations: iteration, finishReason },
    });
    return finish("completed");
  }

  yield stamp({
    type: "run.warning",
    runId,
    timestamp: nowIso(),
    data: {
      code: "max_iterations",
      message: `Reached maxToolIterations=${maxIterations} without final answer.`,
    },
  });
  // Exhausting the iteration budget still ends on run.completed (with a warning),
  // so the terminal reported here matches the event stream.
  yield stamp({
    type: "run.completed",
    runId,
    timestamp: nowIso(),
    data: { iterations: iteration, finishReason: "max_iterations" },
  });
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

/**
 * The optional structured half of a failure: which gate it died at, and whether
 * another attempt could plausibly work. Both are omitted rather than guessed —
 * see {@link AiToolErrorPhase}.
 */
interface ToolFailureDetail {
  phase?: AiToolErrorPhase;
  retryable?: boolean;
}

/** Emit a run.tool.failed event and append the matching error tool message. */
function* failTool(
  stamp: EventStamper,
  runId: string,
  messages: AiChatMessage[],
  tc: AiToolCall,
  errorMessage: string,
  errorCode: string,
  detail: ToolFailureDetail = {},
): Generator<AiRunEvent, void, unknown> {
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

/**
 * Build the Wave-0 balanced error envelope (§0.2) fed to the model on a failure
 * path (tool missing / bad JSON / schema_invalid / exec_failed / cap). The
 * structured `data.errorCode`/`errorMessage` lets the model react precisely;
 * `phase`/`retryable` are added only where this loop actually knows them, so an
 * absent field means "unrecorded", never "false".
 */
function errorEnvelope(
  errorCode: string,
  errorMessage: string,
  detail: ToolFailureDetail = {},
): AiToolEnvelope {
  const data: AiToolErrorData = {
    errorCode,
    errorMessage,
    ...(detail.phase === undefined ? {} : { phase: detail.phase }),
    ...(detail.retryable === undefined ? {} : { retryable: detail.retryable }),
  };
  return {
    ok: false,
    status: "error",
    summary: errorMessage,
    warnings: [],
    truncated: false,
    data,
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
 * `JSON.stringify` that reports instead of throwing. A tool result is arbitrary
 * host data: a BigInt, a cycle, or a `toJSON` that throws are all reachable,
 * and each of them used to escape the generator as a raw rejection.
 */
function safeStringify(
  value: unknown,
): { ok: true; json: string } | { ok: false; error: string } {
  try {
    const json = JSON.stringify(value);
    // `undefined` (and a bare function/symbol) serializes to nothing at all,
    // which is not a message the model can be fed.
    if (json === undefined)
      return { ok: false, error: "value is not representable as JSON" };
    return { ok: true, json };
  } catch (err) {
    return { ok: false, error: errorMessageOf(err) };
  }
}

/** Both JSON views of a successful call: the model's envelope and the UI's raw data. */
function serializeToolResult(
  envelope: AiToolEnvelope,
  data: unknown,
):
  | { ok: true; envelopeJson: string; dataJson: string }
  | { ok: false; error: string } {
  const envelopeJson = safeStringify(envelope);
  if (!envelopeJson.ok) return { ok: false, error: envelopeJson.error };
  const dataJson = safeStringify(data);
  if (!dataJson.ok) return { ok: false, error: dataJson.error };
  return { ok: true, envelopeJson: envelopeJson.json, dataJson: dataJson.json };
}

/** Floor on the data budget, so a huge `summary` can't leave zero room for data. */
const MIN_ENVELOPE_DATA_BYTES = 256;

/**
 * Build the balanced model-facing envelope (Wave 0 §0.2) from a tool result.
 * `data` carries `modelData` when present, else the full `data`. The full payload
 * stays in the success event's `resultJson` for UI/persistence.
 *
 * The envelope is also where the run's output budget is enforced: an uncapped
 * result does not just bloat one message, it is replayed into EVERY later
 * request of the run.
 */
function buildEnvelope(
  result: AiToolResult<unknown>,
  limits: AiToolLimits,
): AiToolEnvelope {
  // A tool that reports status:"partial" keeps "partial" even when ok:false
  // (a partial apply is not a hard error — F5b). Only ok:false WITHOUT a partial
  // status maps to "error".
  const status: AiToolEnvelope["status"] =
    result.status === "partial" ? "partial" : result.ok ? "ok" : "error";
  const envelope: AiToolEnvelope = {
    ok: result.ok,
    status,
    summary: result.summary,
    warnings: result.warnings,
    truncated: result.truncated,
    data: result.modelData ?? result.data,
  };
  const measured = safeStringify(envelope);
  // Unserializable: hand it back as-is; the caller turns that into
  // `result_unserializable` rather than silently capping something it can't read.
  if (!measured.ok) return envelope;
  if (!truncateString(measured.json, limits.maxBytes).truncated)
    return envelope;
  return fitEnvelope(envelope, limits.maxBytes);
}

/**
 * Shrink `data` until the SERIALIZED envelope fits `maxBytes`.
 *
 * One pass is not always enough: a preview is JSON inside JSON, and escaping
 * every quote can nearly double what the first budget assumed. So the budget
 * halves until the whole envelope measures under the cap.
 */
function fitEnvelope(
  envelope: AiToolEnvelope,
  maxBytes: number,
): AiToolEnvelope {
  const shell = safeStringify({ ...envelope, data: null });
  // Bytes, not code units: the budget is a byte budget, and a non-ASCII summary
  // spends 2-4x what its `.length` suggests.
  const overhead = shell.ok ? new TextEncoder().encode(shell.json).length : 0;
  let budget = Math.max(MIN_ENVELOPE_DATA_BYTES, maxBytes - overhead);
  let capped = capEnvelopeData(envelope, budget);
  while (budget > MIN_ENVELOPE_DATA_BYTES) {
    const json = safeStringify(capped);
    if (!json.ok) return capped;
    if (!truncateString(json.json, maxBytes).truncated) return capped;
    budget = Math.max(MIN_ENVELOPE_DATA_BYTES, Math.floor(budget / 2));
    capped = capEnvelopeData(envelope, budget);
  }
  return capped;
}

/** One capping pass: replace `data` with at most `budget` bytes of itself. */
function capEnvelopeData(
  envelope: AiToolEnvelope,
  budget: number,
): AiToolEnvelope {
  const data = envelope.data;
  if (typeof data === "string") {
    return {
      ...envelope,
      truncated: true,
      data: truncateString(data, budget).value,
    };
  }
  const json = safeStringify(data);
  if (!json.ok) return envelope;
  return {
    ...envelope,
    truncated: true,
    // A cut-off object is no longer valid JSON, so the head of its serialization
    // rides as a STRING preview: readable by the model, impossible to mistake
    // for the whole result.
    data: { truncated: true, preview: truncateString(json.json, budget).value },
  };
}

/**
 * A thrown error's own verdict on whether trying again is worth anything.
 *
 * Read off an optional `retryable` property rather than inferred from the error
 * class: only the thrower knows whether the failure was a dead socket or a
 * rejected argument, and defaulting to `false` keeps an unannotated throw from
 * advertising a retry nobody promised.
 */
function retryableOf(err: unknown): boolean {
  return (
    typeof err === "object" &&
    err !== null &&
    "retryable" in err &&
    (err as { retryable: unknown }).retryable === true
  );
}

type ToolExecutionOutcome =
  | { ok: true; value: AiToolResult<unknown> }
  | { ok: false; error: string; retryable: boolean };

async function executeToolSafely(
  tool: AiTool<unknown, unknown>,
  ctx: Parameters<AiTool["execute"]>[0],
  input: unknown,
  defaultTimeoutMs?: number,
): Promise<ToolExecutionOutcome> {
  const declared = tool.definition.timeoutMs;
  const timeoutMs = declared && declared > 0 ? declared : defaultTimeoutMs;
  // No timeout at all: unchanged fast path (every local tool hits this).
  if (!timeoutMs || timeoutMs <= 0) {
    try {
      const value = await tool.execute(ctx, input);
      return { ok: true, value };
    } catch (err) {
      return {
        ok: false,
        error: err instanceof Error ? err.message : String(err),
        retryable: retryableOf(err),
      };
    }
  }
  // Deadline: abort via a controller linked to the run-level signal, so either
  // the run cancelling OR the deadline elapsing aborts the tool's fetch — AND
  // race it, because a tool is free to ignore the signal it was handed. Without
  // the race, an unresponsive remote tool parks the run (and the host's lease)
  // for as long as it feels like.
  const controller = new AbortController();
  const parent = ctx.signal;
  const onParentAbort = () => controller.abort(parent?.reason);
  if (parent) {
    if (parent.aborted) controller.abort(parent.reason);
    else parent.addEventListener("abort", onParentAbort, { once: true });
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  let timedOut = false;
  // An async wrapper so a synchronous throw from execute() becomes a rejection
  // the race can observe; the no-op catch keeps a LATE rejection (the tool
  // settling after the deadline, when nobody is listening) from surfacing as an
  // unhandled one. A late result is dropped: the failure is already reported.
  const running = (async () =>
    tool.execute({ ...ctx, signal: controller.signal }, input))();
  running.catch(() => {});
  try {
    const value = await Promise.race([
      running,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          timedOut = true;
          controller.abort();
          reject(
            new Error(
              `Tool ${tool.definition.name} timed out after ${timeoutMs}ms`,
            ),
          );
        }, timeoutMs);
      }),
    ]);
    return { ok: true, value };
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : String(err),
      // A deadline says nothing about the tool's own retry semantics, and the
      // abort error it produced is the loop's, not the tool's.
      retryable: timedOut ? false : retryableOf(err),
    };
  } finally {
    if (timer !== undefined) clearTimeout(timer);
    if (parent) parent.removeEventListener("abort", onParentAbort);
  }
}
