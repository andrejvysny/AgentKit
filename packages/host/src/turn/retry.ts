import type { AiChatMessage } from "@agentkit/contracts";

/** Which terminal a `runChat` pass reached. */
export type PassTerminal = "completed" | "failed" | "cancelled";

export interface RetryDecisionInput {
  terminal: PassTerminal;
  /** Whether tools were advertised to the provider on the pass that just ran. */
  registryHadTools: boolean;
  /** Distinct tool calls the CURRENT pass has made — see `resetPass`. */
  toolCallCount: number;
  /** Whether the assistant produced any visible text. */
  hadContent: boolean;
}

/** How many provider round-trips a recovery pass may take. One answer, no tools. */
export const RETRY_MAX_TOOL_ITERATIONS = 1;

/**
 * Retry chat-only after a provider failure on a run that had tools staged.
 *
 * Plenty of OpenAI-compatible endpoints accept a plain chat request and fail
 * only when a `tools` array is attached — an unsupported schema keyword, a proxy
 * that rejects the field, a model that was never wired for it. Retrying the same
 * question without tools turns "the assistant is broken" into a usable answer,
 * and costs one request to find out.
 */
export function shouldRetryChatOnly(input: {
  terminal: PassTerminal;
  registryHadTools: boolean;
}): boolean {
  return input.terminal === "failed" && input.registryHadTools;
}

/**
 * Retry once when a completed turn produced nothing at all.
 *
 * A reasoning model can spend a whole turn on hidden reasoning and return no
 * visible content and no tool calls. The turn "succeeded" and the user is
 * looking at an empty bubble, so one plain re-ask is worth more than a correct
 * but useless success.
 */
export function shouldRetryEmptyResponse(input: {
  terminal: PassTerminal;
  toolCallCount: number;
  hadContent: boolean;
}): boolean {
  return (
    input.terminal === "completed" &&
    !input.hadContent &&
    input.toolCallCount === 0
  );
}

/**
 * Drop the tool half of a conversation: tool results and the assistant turns
 * that requested them.
 *
 * A retry that runs WITHOUT tools must not replay them either. Tool results
 * whose requesting assistant turn is gone (or whose tools are no longer
 * advertised) are exactly the orphan `tool_call_id`s a provider rejects — the
 * retry would fail for a new reason, and look like the original failure.
 */
export function filterToolTurns(
  messages: readonly AiChatMessage[],
): AiChatMessage[] {
  return messages.filter(
    (message) =>
      message.role !== "tool" &&
      !(message.toolCalls !== undefined && message.toolCalls.length > 0),
  );
}
