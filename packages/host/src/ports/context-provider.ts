import type { AiContextBinding, AiPromptContextBlock } from "@agentkit/contracts";

/**
 * What the host pins into a chat's context: the objects the model is working on
 * and the system text describing them.
 *
 * Bindings are resolved per run rather than stored on the run because the world
 * moves between turns — a bound document can be deleted, renamed, or go stale,
 * and a run started five minutes later must see that, not a snapshot.
 */
export interface ContextProvider {
  listBindings(
    chatId: string,
    signal?: AbortSignal,
  ): Promise<AiContextBinding[]>;
  /** Fully composed system prompt for this chat, when the host builds its own. */
  systemPrompt?(chatId: string, signal?: AbortSignal): Promise<string | null>;
  /** Blocks for a host composing through core's `composeSystemPrompt`. */
  promptBlocks?(
    chatId: string,
    signal?: AbortSignal,
  ): Promise<AiPromptContextBlock[]>;
  /** Re-check binding health before a run reads them. */
  refresh?(chatId: string, signal?: AbortSignal): Promise<void>;
}
