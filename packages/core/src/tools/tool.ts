import type {
  AiContextBinding,
  AiToolDefinition,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";
import type { AgentSuspendRequest } from "../runs/session-seams.js";

export interface AiToolExecutionContext {
  runId: string;
  chatId?: string;
  /**
   * What this run writes to, as the host defined it (`TaskRecord.scopeId`) —
   * usually the chat, but a host writing a shared document scopes on the
   * document instead, so two chats editing it are serialized against each other.
   *
   * A write tool needs it: the scope is the namespace its idempotency key lives
   * in and the thing its proposal is staged against. Without it a tool can only
   * guess (hardcode a constant, or fall back to `chatId` and get the
   * shared-document case wrong), so the run's own answer is threaded through
   * `RunChatInput.scopeId` rather than re-derived downstream.
   */
  scopeId?: string;
  userId?: string;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  signal?: AbortSignal;
  metadata?: Record<string, unknown>;
  /**
   * Present only when the loop is suspendable. Calling it (before `execute`
   * settles) pauses the run for human input: this call's result is then the
   * human's later answer, and whatever `execute` returns is discarded. Only
   * the first call per step is honoured.
   */
  suspend?(request: AgentSuspendRequest): void;
}

export interface AiTool<TInput = unknown, TOutput = unknown> {
  definition: AiToolDefinition;
  execute(
    ctx: AiToolExecutionContext,
    input: TInput,
  ): Promise<AiToolResult<TOutput>>;
}
