import type { AiProviderConnectionIdentity } from "./responses-types.js";
import type { AiRunEvent } from "@agentkit/contracts";
import type {
  AiChatMessage,
  AiProviderContinuation,
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiToolDefinition,
} from "@agentkit/contracts";

export interface AiChatRequest {
  /** Reserve a durable request budget immediately before each transport dispatch. */
  beforeRequest?: () => Promise<void>;
  /** Trusted host continuation; never populated from a public request body. */
  continuation?: AiProviderContinuation;
  continuationScope?: { chatId: string; branchId: string };
  continuationRequired?: boolean;
  onContinuation?: (state: AiProviderContinuation) => void | Promise<void>;
  /** Valid provider stream activity, including non-visible reasoning/tool fragments. */
  onActivity?: () => void;
  runId: string;
  model: string;
  messages: AiChatMessage[];
  tools?: AiToolDefinition[];
  temperature?: number;
  maxOutputTokens?: number;
  signal?: AbortSignal;
}

export interface AiProviderClient {
  /** Calls beforeRequest for each network attempt, including internal retries. */
  readonly tracksTransportRequests?: boolean;
  /** Trusted transport metadata, when the client requires durable provider state. */
  readonly protocol?: string;
  readonly connectionIdentity?: AiProviderConnectionIdentity;
  id: string;
  kind: AiProviderKind;
  capabilities(
    signal?: AbortSignal,
    model?: string,
  ): Promise<AiProviderCapabilities>;
  listModels(signal?: AbortSignal): Promise<AiProviderModel[]>;
  streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent>;
}
