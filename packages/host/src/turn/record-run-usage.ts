import type { AiRunEvent } from "@agentkit/contracts";
import type {
  RunProjectionContext,
  RunProjectionState,
  RunProjectorDeps,
} from "./projection.js";

/** Meter only after durable projection, outside its database transaction. */
export async function recordRunUsage(
  deps: Pick<RunProjectorDeps, "usage" | "logger">,
  ctx: RunProjectionContext,
  state: RunProjectionState,
  event: Extract<AiRunEvent, { type: "run.usage" }>,
): Promise<void> {
  const task = ctx.task;
  try {
    await deps.usage?.record({
      runId: task.taskId,
      callId: event.data.callId,
      attempt: event.data.attempt,
      // The provider that PRODUCED the numbers wins over the one the turn
      // started on: a routed run bills a switch to the provider it switched
      // to, and an event that names none falls back to what the projector
      // was told to bill against.
      providerId: event.data.providerId ?? state.providerId ?? "",
      model: event.data.model,
      finalForCall: event.data.finalForCall,
      source: event.data.source,
      step: event.data.step,
      ...(event.data.promptTokens === undefined
        ? {}
        : { promptTokens: event.data.promptTokens }),
      ...(event.data.completionTokens === undefined
        ? {}
        : { completionTokens: event.data.completionTokens }),
      ...(event.data.totalTokens === undefined
        ? {}
        : { totalTokens: event.data.totalTokens }),
      ...(event.data.cachedPromptTokens === undefined
        ? {}
        : { cachedPromptTokens: event.data.cachedPromptTokens }),
      ...(event.data.reasoningTokens === undefined
        ? {}
        : { reasoningTokens: event.data.reasoningTokens }),
      at: event.timestamp,
    });
  } catch (err) {
    deps.logger?.warn("usage record failed", {
      taskId: task.taskId,
      callId: event.data.callId,
      error: err instanceof Error ? err.message : String(err),
    });
  }
}
