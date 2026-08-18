import type { AiToolCall } from "./tool.js";

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
