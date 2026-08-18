import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import type {
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import { nowIso, createTestEventStamper } from "./stamp.js";

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
  /**
   * When true, each turn also emits a `run.usage` draft after its message
   * events, the way a real streaming provider reports token accounting for the
   * call it just made.
   */
  emitUsage = false;
  /** How many times streamChat has been invoked — i.e. provider round-trips. */
  callCount = 0;
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
    this.callCount++;
    const turnNumber = this.turnIndex;
    const turn = this.turns[this.turnIndex] ?? { steps: [] };
    this.turnIndex++;
    const runId = input.runId;
    // A mock still has to emit valid events: the base fields are part of the
    // contract, not decoration the run-loop adds later.
    const stamp = createTestEventStamper();
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
    if (this.emitUsage) {
      yield stamp({
        type: "run.usage",
        runId,
        timestamp: nowIso(),
        data: {
          callId: `call-usage-${turnNumber}`,
          attempt: 1,
          step: 0,
          model: input.model,
          promptTokens: 10,
          completionTokens: 5,
          totalTokens: 15,
          source: "stream",
          finalForCall: true,
        },
      });
    }
  }
}
