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
      providerId: state.providerId ?? "",
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
