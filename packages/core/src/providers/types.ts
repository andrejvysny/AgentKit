import type { AiRunEvent } from "../runs/events.js";
import type { AiToolCall, AiToolDefinition } from "../tools/types.js";

export type AiProviderKind =
  | "openai"
  | "openrouter"
  | "openai-compatible"
  | "lmstudio"
  | "omlx"
  | "openpcb-cloud";

export interface AiProviderConfig {
  id: string;
  label: string;
  kind: AiProviderKind;
  baseUrl: string;
  apiKey?: string;
  defaultModel: string;
  enabled: boolean;
  metadata?: Record<string, unknown>;
}

export interface AiProviderCapabilities {
  streaming: boolean;
  toolCalling: boolean;
  modelList: boolean;
  vision?: boolean;
  jsonMode?: boolean;
  maxContextTokens?: number;
  checkedAt?: string;
  warning?: string;
}

export interface AiProviderModel {
  providerId: string;
  modelId: string;
  displayName: string | null;
  contextWindowTokens?: number;
  supportsToolCalling?: boolean;
  fetchedAt: string;
}

export type AiChatRole = "system" | "user" | "assistant" | "tool";

export interface AiChatMessage {
  role: AiChatRole;
  content: string;
  name?: string;
  toolCallId?: string;
  toolCalls?: AiToolCall[];
  metadata?: Record<string, unknown>;
}

export interface AiChatRequest {
  runId: string;
  model: string;
  messages: AiChatMessage[];
  tools?: AiToolDefinition[];
  temperature?: number;
  maxOutputTokens?: number;
  signal?: AbortSignal;
}

export interface AiChatTurnResult {
  content: string;
  toolCalls: AiToolCall[];
  finishReason?: string;
}

export interface AiProviderClient {
  id: string;
  kind: AiProviderKind;
  capabilities(
    signal?: AbortSignal,
    model?: string,
  ): Promise<AiProviderCapabilities>;
  listModels(signal?: AbortSignal): Promise<AiProviderModel[]>;
  streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent>;
}
