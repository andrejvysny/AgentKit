/** One model request: the JS provider returns the final assistant result. */
export interface ProviderRequest {
  runId: string;
  model: string;
  messages: unknown[];
  tools: unknown[];
  temperature?: number | null;
  maxOutputTokens?: number | null;
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
