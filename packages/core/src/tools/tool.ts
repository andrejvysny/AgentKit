import type {
  AiContextBinding,
  AiToolDefinition,
  AiToolLimits,
  AiToolResult,
} from "@agentkit/contracts";

export interface AiToolExecutionContext {
  runId: string;
  chatId?: string;
  userId?: string;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  signal?: AbortSignal;
  metadata?: Record<string, unknown>;
}

export interface AiTool<TInput = unknown, TOutput = unknown> {
  definition: AiToolDefinition;
  execute(
    ctx: AiToolExecutionContext,
    input: TInput,
  ): Promise<AiToolResult<TOutput>>;
}
