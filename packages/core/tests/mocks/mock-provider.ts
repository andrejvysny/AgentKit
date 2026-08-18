import type {
  AiChatRequest,
  AiProviderCapabilities,
  AiProviderClient,
  AiProviderKind,
  AiProviderModel,
} from "../../src/providers/types.js";
import type { AiRunEvent } from "../../src/runs/events.js";
import { nowIso } from "../../src/ids.js";

export type MockScriptStep =
  | { kind: "text"; content: string }
  | {
      kind: "tool_call";
      toolCallId: string;
      name: string;
      argumentsJson: string;
    };

export interface MockTurn {
  steps: MockScriptStep[];
  finishReason?: string;
  reasoning?: string;
}

export class MockProviderClient implements AiProviderClient {
  readonly id = "mock";
  readonly kind: AiProviderKind = "openai-compatible";
  private turns: MockTurn[] = [];
  private turnIndex = 0;
  models: AiProviderModel[] = [];
  caps: AiProviderCapabilities = {
    streaming: true,
    toolCalling: true,
    modelList: true,
  };

  setScript(turns: MockTurn[]) {
    this.turns = turns;
    this.turnIndex = 0;
  }

  async capabilities(): Promise<AiProviderCapabilities> {
    return this.caps;
  }

  async listModels(): Promise<AiProviderModel[]> {
    return this.models;
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const turn = this.turns[this.turnIndex] ?? { steps: [] };
    this.turnIndex++;
    const runId = input.runId;
    yield {
      type: "run.started",
      runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    };
    let content = "";
    let toolCount = 0;
    for (const step of turn.steps) {
      if (step.kind === "text") {
        content += step.content;
        yield {
          type: "run.message.delta",
          runId,
          timestamp: nowIso(),
          data: { delta: step.content },
        };
      } else {
        toolCount++;
      }
    }
    yield {
      type: "run.message.completed",
      runId,
      timestamp: nowIso(),
      data: {
        content,
        toolCallCount: toolCount,
        reasoningContent: turn.reasoning,
        finishReason: turn.finishReason,
      },
    };
    for (const step of turn.steps) {
      if (step.kind === "tool_call") {
        yield {
          type: "run.tool.requested",
          runId,
          timestamp: nowIso(),
          data: {
            toolCallId: step.toolCallId,
            toolName: step.name,
            argumentsJson: step.argumentsJson,
          },
        };
      }
    }
  }
}
