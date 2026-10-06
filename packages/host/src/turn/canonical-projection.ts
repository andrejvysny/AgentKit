import type { AiRunEvent } from "@agentkit/contracts";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { RunProjectionContext, RunProjectionState } from "./projection.js";

export async function canonicalMessage(
  store: AssistantStore,
  ctx: RunProjectionContext,
  state: RunProjectionState,
  event: Extract<AiRunEvent, { type: "run.message.completed" }>,
  append: (
    input: Parameters<AssistantStore["conversations"]["appendMessage"]>[0],
  ) => Promise<import("../ports/conversation-store.js").MessageRecord>,
): Promise<void> {
  const toolCalls = event.data.toolCalls ?? state.announcedToolCalls;
  state.announcedToolCalls = [];
  const record = await append({
    chatId: state.chatId,
    runId: ctx.task.taskId,
    role: "assistant",
    content: event.data.content,
    ...(toolCalls.length ? { toolCalls } : {}),
    parentMessageId: state.lastMessageId,
    activate: false,
    metadata: { internal: true, canonicalProviderTurn: true },
  });
  state.lastMessageId = record.id;
  for (const call of toolCalls) state.toolCallIds.add(call.id);
  state.content = event.data.toolCallCount > 0 ? "" : event.data.content;
  state.streamed = false;
  state.unflushedDeltas = 0;
  await store.conversations.updateMessage(
    state.assistantMessageId,
    { content: state.content },
    { taskId: ctx.task.taskId, leaseToken: ctx.leaseToken },
  );
}
