import type { AiChatRequest, AiProviderClient } from "../../src/providers/client.js";
import type {
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import { nowIso } from "../../src/ids.js";

/**
 * A non-streaming provider mock: emits a single `run.message.completed` carrying
 * `content` + `toolCalls` (no deltas, no separate `run.tool.requested`). Mirrors
 * providers that return one completed payload instead of an SSE token stream.
 */
export interface CompletedTurn {
  content?: string;
  toolCalls?: AiToolCall[];
  finishReason?: string;
}

export class CompletedOnlyProviderClient implements AiProviderClient {
  readonly id = "mock-completed";
  readonly kind: AiProviderKind = "openai-compatible";
  private turns: CompletedTurn[] = [];
  private turnIndex = 0;

  setScript(turns: CompletedTurn[]) {
    this.turns = turns;
    this.turnIndex = 0;
  }

  async capabilities(): Promise<AiProviderCapabilities> {
    return { streaming: false, toolCalling: true, modelList: true };
  }

  async listModels(): Promise<AiProviderModel[]> {
    return [];
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const turn = this.turns[this.turnIndex] ?? {};
    this.turnIndex++;
    const runId = input.runId;
    yield {
      type: "run.started",
      runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    };
    const toolCalls = turn.toolCalls ?? [];
    yield {
      type: "run.message.completed",
      runId,
      timestamp: nowIso(),
      data: {
        content: turn.content ?? "",
        toolCallCount: toolCalls.length,
        toolCalls: toolCalls.length > 0 ? toolCalls : undefined,
        finishReason: turn.finishReason,
      },
    };
  }
}
