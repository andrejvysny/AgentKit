import type {
  AiProviderCapabilities,
  AiProviderModel,
  AiRunEvent,
  AiToolDefinition,
  AiToolResult,
} from "@agentkit/contracts";
import { CONTRACT_VERSION } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import { resolveToolLimits } from "@agentkit/core";

export function ev(type: string, data: unknown): AiRunEvent {
  return {
    type: type as AiRunEvent["type"],
    runId: "fixture-run",
    timestamp: "2026-01-01T00:00:00.000Z",
    contractVersion: CONTRACT_VERSION,
    seq: 0,
    data,
  } as AiRunEvent;
}

/** One scripted provider turn: events to yield, then optionally fail. */
export interface ScriptedTurn {
  events?: AiRunEvent[];
  error?: Error;
  /** Yield this many events, then throw (models a mid-stream failure). */
  failAfter?: number;
}

/**
 * Provider client driven by a fixed script. `calls` records every request so
 * tests can assert what the wrapper passed through.
 */
export class ScriptedClient implements AiProviderClient {
  readonly id = "scripted";
  readonly kind = "openai-compatible" as const;
  readonly calls: AiChatRequest[] = [];
  models: AiProviderModel[] = [
    {
      providerId: "scripted",
      modelId: "m",
      displayName: "M",
      fetchedAt: "2026-01-01T00:00:00.000Z",
    },
  ];
  caps: AiProviderCapabilities = {
    streaming: true,
    toolCalling: true,
    modelList: true,
  };

  constructor(private turns: ScriptedTurn[]) {}

  async capabilities(): Promise<AiProviderCapabilities> {
    return this.caps;
  }
  async listModels(): Promise<AiProviderModel[]> {
    return this.models;
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const turn = this.turns[this.calls.length];
    this.calls.push(input);
    if (!turn) throw new Error("scripted client: script exhausted");
    const events = turn.events ?? [];
    const limit = turn.failAfter ?? events.length;
    for (let i = 0; i < Math.min(limit, events.length); i += 1)
      yield events[i] as AiRunEvent;
    if (turn.error) throw turn.error;
  }
}

/**
 * Client that fails loudly if replay ever touches it. Identity mirrors
 * `ScriptedClient` because replay matching includes provider identity — the
 * same configured client is what a real replay test would construct.
 */
export class ThrowingClient implements AiProviderClient {
  readonly id = "scripted";
  readonly kind = "openai-compatible" as const;
  touched = false;

  private fail(): never {
    this.touched = true;
    throw new Error("live provider reached during replay");
  }
  async capabilities(): Promise<AiProviderCapabilities> {
    this.fail();
  }
  async listModels(): Promise<AiProviderModel[]> {
    this.fail();
  }
  streamChat(): AsyncIterable<AiRunEvent> {
    return this.fail();
  }
}

export function toolDefinition(name: string): AiToolDefinition {
  return {
    name,
    version: "1.0.0",
    description: `test tool ${name}`,
    capability: "test",
    effect: "read",
    inputSchema: { type: "object", additionalProperties: true },
  };
}

export function makeTool(
  name: string,
  execute: AiTool["execute"],
  overrides: Partial<AiToolDefinition> = {},
): AiTool {
  return {
    definition: { ...toolDefinition(name), ...overrides },
    execute,
  };
}

export function okResult(data: unknown = null): AiToolResult {
  return {
    ok: true,
    data,
    sources: [{ id: "src-1", kind: "file", label: "a.txt" }],
    warnings: [],
    truncated: false,
    limits: resolveToolLimits({ preference: "large" }),
  };
}

export function textTurn(content: string): AiRunEvent[] {
  return [
    ev("run.message.requested", {}),
    ev("run.message.delta", { delta: content.slice(0, 3) }),
    ev("run.message.delta", { delta: content.slice(3) }),
    ev("run.message.completed", { content, finishReason: "stop" }),
  ];
}

export function toolCallTurn(
  calls: { id: string; name: string; args: unknown }[],
): AiRunEvent[] {
  return [
    ev("run.message.requested", {}),
    ...calls.map((c) =>
      ev("run.tool.requested", {
        toolCallId: c.id,
        toolName: c.name,
        argumentsJson: JSON.stringify(c.args),
      }),
    ),
    ev("run.message.completed", {
      content: "",
      finishReason: "tool_calls",
      toolCallCount: calls.length,
    }),
  ];
}

export async function collect(
  iterable: AsyncIterable<AiRunEvent>,
): Promise<AiRunEvent[]> {
  const events: AiRunEvent[] = [];
  for await (const event of iterable) events.push(event);
  return events;
}
