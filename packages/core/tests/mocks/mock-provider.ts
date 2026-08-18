import type { AiChatRequest, AiProviderClient } from "../../src/providers/client.js";
import type {
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import { nowIso } from "../../src/ids.js";
import { createEventStamper } from "../../src/events.js";

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
  /**
   * When true, tool calls are ALSO echoed into `run.message.completed.data.toolCalls`
   * (on top of the `run.tool.requested` events), the way a real streaming provider
   * reports them twice.
   */
  echoToolCallsIntoCompleted = false;
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
    // A mock still has to emit valid events: the base fields are part of the
    // contract, not decoration the run-loop adds later.
    const stamp = createEventStamper();
    yield stamp({
      type: "run.started",
      runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: input.tools?.length ?? 0 },
    });
    let content = "";
    const toolCalls: AiToolCall[] = [];
    for (const step of turn.steps) {
      if (step.kind === "text") {
        content += step.content;
        yield stamp({
          type: "run.message.delta",
          runId,
          timestamp: nowIso(),
          data: { delta: step.content },
        });
      } else {
        toolCalls.push({
          id: step.toolCallId,
          name: step.name,
          argumentsJson: step.argumentsJson,
        });
      }
    }
    yield stamp({
      type: "run.message.completed",
      runId,
      timestamp: nowIso(),
      data: {
        content,
        toolCallCount: toolCalls.length,
        toolCalls:
          this.echoToolCallsIntoCompleted && toolCalls.length > 0
            ? toolCalls
            : undefined,
        reasoningContent: turn.reasoning,
        finishReason: turn.finishReason,
      },
    });
    for (const step of turn.steps) {
      if (step.kind === "tool_call") {
        yield stamp({
          type: "run.tool.requested",
          runId,
          timestamp: nowIso(),
          data: {
            toolCallId: step.toolCallId,
            toolName: step.name,
            argumentsJson: step.argumentsJson,
          },
        });
      }
    }
  }
}
