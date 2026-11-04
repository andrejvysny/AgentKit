/** One model request: the JS provider returns the final assistant result. */
export interface ProviderRequest {
  runId: string;
  model: string;
  messages: unknown[];
  tools: unknown[];
  temperature?: number | null;
  maxOutputTokens?: number | null;
  /**
   * Aborts when the host has given up on this callback — run cancellation
   * (`cancel` notification) or `close()` (`discard`/drain). The provider is
   * expected to thread it into its fetch; the bridge settles the call as
   * cancelled regardless, so honoring the signal is what stops the network,
   * not what stops the run.
   */
  signal?: AbortSignal;
}

export interface ProviderToolCall {
  id?: string;
  name: string;
  arguments?: unknown;
}

export interface ProviderResult {
  content?: string;
  toolCalls?: ProviderToolCall[];
  finishReason?: string;
  usage?: Record<string, unknown> | null;
}

export type ProviderFunction = (
  request: ProviderRequest,
) => Promise<ProviderResult> | ProviderResult;

/** One tool execution: the JS tool returns the host tool-result shape. */
export interface ToolCallRequest {
  name: string;
  toolCallId: string | null;
  attemptId: string | null;
  runId: string;
  input: unknown;
  /**
   * Aborts when the host has given up on this invocation: run cancellation
   * delivers the bridge `cancel` notification, and `close()` aborts
   * everything still in flight. Consumers that own side effects (the coding
   * pack spawns process groups) MUST thread this into what they start; a
   * tool that ignores it keeps effects running after cancellation, and the
   * runtime has already reported the call as settled.
   */
  signal?: AbortSignal;
}

export interface ToolResultJson {
  ok?: boolean;
  data?: unknown;
  sources?: unknown[];
  warnings?: string[];
  truncated?: boolean;
  limits?: { profile: string; maxBytes: number; maxItems?: number };
  modelData?: unknown;
  summary?: string;
  status?: string;
}

export interface ToolDefinitionJson {
  name: string;
  version?: string;
  effect?: "read" | "write";
  capability?: string;
  description?: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
  timeoutMs?: number;
}

export type ToolRunFunction = (
  input: unknown,
  request: ToolCallRequest,
) => Promise<ToolResultJson | unknown> | ToolResultJson | unknown;
