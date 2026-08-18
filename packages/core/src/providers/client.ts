import type { AiRunEvent } from "@agentkit/contracts";
import type {
  AiChatMessage,
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiToolDefinition,
} from "@agentkit/contracts";

export interface AiChatRequest {
  runId: string;
  model: string;
  messages: AiChatMessage[];
  tools?: AiToolDefinition[];
  temperature?: number;
  maxOutputTokens?: number;
  signal?: AbortSignal;
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
