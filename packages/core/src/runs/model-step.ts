import type {
  AiChatMessage,
  AiRunEvent,
  AiToolCall,
  AiToolDefinition,
} from "@agentkit/contracts";
import type { EventStamper } from "../events.js";
import { nowIso } from "../ids.js";
import { expandToolResultContent } from "../messages/tool-content.js";
import type { AiChatRequest } from "../providers/client.js";
import { dedupeToolCallIds } from "../tools/tool-calls.js";
import { errorMessageOf, isAbortError } from "./errors.js";
import {
  defaultRetrySleep,
  effectiveMaxAttempts,
  isRetryableProviderCode,
  retryDelayMs,
} from "./provider-retry.js";
import type { RunChatInput } from "./run-types.js";

export interface ModelStepContext {
  input: RunChatInput;
  runId: string;
  iteration: number;
  /** Read-only here; the loop owns and mutates it. */
  messages: readonly AiChatMessage[];
  toolDefinitions: AiToolDefinition[];
  stamp: EventStamper;
  /**
   * The loop owns `continuation` (onContinuation mutates it across
   * iterations), so it is read at request time through this getter.
   */
  getContinuation: () => AiChatRequest["continuation"];
  onContinuation: AiChatRequest["onContinuation"];
}

export type ModelStepOutcome =
  | { kind: "cancelled" }
  | { kind: "failed" }
  | {
      kind: "response";
      content: string;
      toolCalls: AiToolCall[];
      duplicateIds: string[];
      announcedToolCallIds: Set<string>;
      finishReason?: string;
      sawNoToolCallWarning: boolean;
    };

type AttemptOutcome = ModelStepOutcome | { kind: "retry"; code: string };

/**
 * Stream one provider round-trip, re-yielding its events stamped. Returns the
 * assembled turn, or "cancelled"/"failed" once the matching terminal event has
 * been yielded. With `input.providerRetry`, a transient failure before any
 * visible output is retried (bounded, backed off) instead of failing the run.
 */
export async function* streamModelStep(
  ctx: ModelStepContext,
): AsyncGenerator<AiRunEvent, ModelStepOutcome, unknown> {
  const policy = ctx.input.providerRetry;
  const maxAttempts = effectiveMaxAttempts(policy);
  for (let attempt = 1; ; attempt++) {
    const canRetry = policy !== undefined && attempt < maxAttempts;
    const outcome = yield* streamModelAttempt(ctx, canRetry, attempt > 1);
    if (outcome.kind !== "retry" || policy === undefined) {
      return outcome as ModelStepOutcome;
    }
    const delayMs = retryDelayMs(policy, attempt);
    yield ctx.stamp({
      type: "run.warning",
      runId: ctx.runId,
      timestamp: nowIso(),
      data: {
        code: "provider_retry",
        message: `Provider request failed (${outcome.code}); retrying in ${
          Math.round(delayMs / 100) / 10
        }s (attempt ${attempt + 1} of ${maxAttempts}).`,
      },
    });
    try {
      await (policy.sleep ?? defaultRetrySleep)(delayMs, ctx.input.signal);
    } catch (err) {
      // A sleep seam that rejects is only meaningful as an abort; otherwise
      // the backoff is skipped and the retry proceeds.
      if (!ctx.input.signal?.aborted && !isAbortError(err)) continue;
    }
    if (ctx.input.signal?.aborted) {
      yield ctx.stamp({
        type: "run.cancelled",
        runId: ctx.runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
      return { kind: "cancelled" };
    }
  }
}

async function* streamModelAttempt(
  ctx: ModelStepContext,
  canRetry: boolean,
  isRetryAttempt: boolean,
): AsyncGenerator<AiRunEvent, AttemptOutcome, unknown> {
  const {
    input,
    runId,
    iteration,
    messages,
    toolDefinitions,
    stamp,
    getContinuation,
    onContinuation,
  } = ctx;
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
  // Output a consumer may already have shown or acted on; once set, a failure
  // is never retried.
  let visibleOutput = false;
  let retryCode: string | undefined;
  const mayRetry = (code: string | undefined): code is string =>
    canRetry &&
    !visibleOutput &&
    !input.signal?.aborted &&
    isRetryableProviderCode(code);

  // A provider client is expected to normalize its own transport failures into
  // run.failed, but a THROW still escapes — a bad JSON body, a socket reset, a
  // bug in a third-party client. Unwrapped it would blow past every consumer
  // as a raw rejection, leaving the run with no terminal event at all. Catch it
  // here and turn it into the one terminal event the contract promises.
  try {
    // Read at request time: onContinuation moves it between iterations.
    const continuation = getContinuation();
    for await (const event of input.client.streamChat({
      runId,
      model: input.model,
      messages: expandToolResultContent(messages),
      tools: toolDefinitions.length > 0 ? toolDefinitions : undefined,
      temperature: input.temperature,
      maxOutputTokens: input.maxOutputTokens,
      reasoningEffort: input.reasoningEffort,
      signal: input.signal,
      continuation,
      continuationScope: input.continuationScope,
      continuationRequired:
        input.continuationRequired || continuation !== undefined,
      onContinuation,
      runContent: input.runContent,
    })) {
      // Re-yield provider events with the canonical runId.
      const stamped = { ...event, runId } as AiRunEvent;
      if (retryCode !== undefined) {
        // Failed attempt being discarded: keep only its token accounting.
        if (stamped.type === "run.usage") {
          yield stamp({
            ...stamped,
            data: { ...stamped.data, step: iteration },
          });
        }
        continue;
      }
      switch (stamped.type) {
        case "run.message.reasoning-delta":
          visibleOutput = true;
          yield stamp(stamped);
          break;
        case "run.message.delta":
          visibleOutput = true;
          deltaContent += stamped.data.delta;
          yield stamp(stamped);
          break;
        case "run.message.completed":
          visibleOutput = true;
          completedContent = stamped.data.content;
          if (stamped.data.toolCalls && stamped.data.toolCalls.length > 0) {
            completedToolCalls = stamped.data.toolCalls;
          }
          turnFinishReason = stamped.data.finishReason;
          yield stamp(stamped);
          break;
        case "run.tool.requested":
          visibleOutput = true;
          requestedToolCalls.push({
            id: stamped.data.toolCallId,
            name: stamped.data.toolName,
            argumentsJson: stamped.data.argumentsJson,
          });
          announcedToolCallIds.add(stamped.data.toolCallId);
          yield stamp(stamped);
          break;
        case "run.failed":
          if (mayRetry(stamped.data.errorCode)) {
            retryCode = stamped.data.errorCode;
            break;
          }
          // The provider already emitted run.failed; record it and don't re-emit.
          turnFailed = true;
          yield stamp(stamped);
          break;
        case "run.cancelled":
          yield stamp(stamped);
          return { kind: "cancelled" };
        case "run.started":
          if (iteration === 1 && !isRetryAttempt) yield stamp(stamped);
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
          if (
            stamped.data.code === "multimodal_flattened" &&
            (iteration > 1 || isRetryAttempt)
          )
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
      return { kind: "cancelled" };
    }
    if (mayRetry("provider_error")) {
      return { kind: "retry", code: "provider_error" };
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
    return { kind: "failed" };
  }

  if (retryCode !== undefined) return { kind: "retry", code: retryCode };

  if (turnFailed) {
    // Provider already yielded run.failed during the stream — don't double-emit.
    return { kind: "failed" };
  }

  const assembledToolCalls = dedupeToolCallIds(
    completedToolCalls ?? requestedToolCalls,
  );
  return {
    kind: "response",
    content: completedContent ?? deltaContent,
    toolCalls: assembledToolCalls.calls,
    duplicateIds: assembledToolCalls.duplicateIds,
    announcedToolCallIds,
    finishReason: turnFinishReason,
    sawNoToolCallWarning,
  };
}
