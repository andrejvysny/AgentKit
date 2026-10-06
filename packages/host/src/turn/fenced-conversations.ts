import type { AssistantStore } from "../ports/assistant-store.js";
import type { ConversationStore } from "../ports/conversation-store.js";
import type { RunProjectionContext } from "./projection.js";

/** Bind ownership to every mutation, rather than checking before an awaited write. */
export function fencedConversations(
  store: AssistantStore,
  ctx: RunProjectionContext,
): Pick<ConversationStore, "appendMessage" | "updateMessage"> {
  const fence = { taskId: ctx.task.taskId, leaseToken: ctx.leaseToken };
  return {
    appendMessage: (input) => store.conversations.appendMessage(input, fence),
    updateMessage: (messageId, patch) =>
      store.conversations.updateMessage(messageId, patch, fence),
  };
}
