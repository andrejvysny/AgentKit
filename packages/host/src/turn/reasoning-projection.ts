import type { AssistantStore } from "../ports/assistant-store.js";
import type { RunProjectionContext, RunProjectionState } from "./projection.js";
import type { Clock } from "../ports/system.js";

/** Text and public reasoning share the same bounded durable-write cadence. */
export async function flushProjectionContent(
  store: AssistantStore,
  ctx: RunProjectionContext,
  state: RunProjectionState,
  clock: Clock,
): Promise<void> {
  if (state.unflushedDeltas === 0) return;
  await store.conversations.updateMessage(
    state.assistantMessageId,
    {
      content: state.content,
      ...(await reasoningPatch(store, state)),
    },
    { taskId: ctx.task.taskId, leaseToken: ctx.leaseToken },
  );
  // Advance only after the write succeeds so failures remain replayable.
  state.unflushedDeltas = 0;
  state.lastFlushAtMs = clock.now().getTime();
}

/** Preserve host-owned metadata while updating the model's public reasoning. */
export async function reasoningPatch(
  store: AssistantStore,
  state: RunProjectionState,
): Promise<{ metadata?: Record<string, unknown> }> {
  const message = await store.conversations.getMessage(
    state.assistantMessageId,
  );
  if (
    !state.reasoningContent &&
    message?.metadata.reasoningContent === undefined
  )
    return {};
  return {
    metadata: {
      ...message?.metadata,
      reasoningContent: state.reasoningContent,
    },
  };
}

/** The completed value replaces this provider turn's deltas, never duplicates them. */
export async function completeReasoning(
  store: AssistantStore,
  ctx: RunProjectionContext,
  state: RunProjectionState,
  completed: string | undefined,
): Promise<void> {
  if (completed !== undefined) {
    state.reasoningContent =
      state.reasoningContent.slice(0, state.reasoningTurnStart) + completed;
  }
  state.reasoningTurnStart = state.reasoningContent.length;
  const patch = await reasoningPatch(store, state);
  if (patch.metadata) {
    await store.conversations.updateMessage(state.assistantMessageId, patch, {
      taskId: ctx.task.taskId,
      leaseToken: ctx.leaseToken,
    });
  }
}
