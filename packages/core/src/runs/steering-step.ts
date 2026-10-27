import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { EventStamper } from "../events.js";
import { nowIso } from "../ids.js";
import { DEFAULT_HOOK_TIMEOUT_MS, invokeHook } from "./loop-hooks.js";
import type { AgentSteeringSource } from "./session-seams.js";

export interface SteeringStepContext {
  source: AgentSteeringSource;
  runId: string;
  /** The 1-based step the steers are placed before. */
  step: number;
  stamp: EventStamper;
  messages: AiChatMessage[];
  /** Mutated: ids already placed in this run, so a redelivery is a no-op. */
  applied: Set<string>;
  hookTimeoutMs?: number;
  signal?: AbortSignal;
}

function warning(
  ctx: SteeringStepContext,
  code: string,
  message: string,
): AiRunEvent {
  return ctx.stamp({
    type: "run.warning",
    runId: ctx.runId,
    timestamp: nowIso(),
    data: { code, message },
  });
}

/**
 * Poll the steering source and place its user messages onto `messages`.
 *
 * Never fails the run: a throwing, hanging or aborted source just means no
 * steer this step (they stay pending with the host). Each placement is
 * announced BEFORE the message is pushed, so the event log never claims an
 * input the history does not hold.
 */
export async function* applySteering(
  ctx: SteeringStepContext,
): AsyncGenerator<AiRunEvent, void, unknown> {
  const outcome = await invokeHook(
    (c: { runId: string; step: number; signal?: AbortSignal }) =>
      ctx.source.pending(c),
    { runId: ctx.runId, step: ctx.step, signal: ctx.signal },
    ctx.hookTimeoutMs ?? DEFAULT_HOOK_TIMEOUT_MS,
    ctx.signal,
  );
  if (!outcome.ok) {
    if (outcome.reason === "aborted") return;
    yield warning(
      ctx,
      outcome.reason === "threw" ? "hook_failed" : "hook_timeout",
      outcome.reason === "threw"
        ? "Steering source threw; no steering applied this step."
        : "Steering source timed out; no steering applied this step.",
    );
    return;
  }
  for (const item of outcome.value) {
    if (ctx.applied.has(item.inputId)) continue;
    if (item.message.role !== "user") {
      yield warning(
        ctx,
        "steering_rejected",
        `Steering input ${item.inputId} rejected: only role "user" may steer.`,
      );
      continue;
    }
    ctx.applied.add(item.inputId);
    yield ctx.stamp({
      type: "run.input.applied",
      runId: ctx.runId,
      timestamp: nowIso(),
      data: { inputId: item.inputId, kind: "steer", step: ctx.step },
    });
    ctx.messages.push({ ...item.message });
  }
}
