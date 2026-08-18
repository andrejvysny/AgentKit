import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { MockProviderClient } from "./mocks/mock-provider.js";
import type { AiChatMessage } from "../src/providers/types.js";
import type { AiTool } from "../src/tools/types.js";
import type { AiRunEvent } from "../src/runs/events.js";

async function collect(
  input: Parameters<typeof runChat>[0],
): Promise<AiRunEvent[]> {
  const out: AiRunEvent[] = [];
  for await (const e of runChat(input)) out.push(e);
  return out;
}

function makeEchoTool(): AiTool<{ text: string }, { echoed: string }> {
  return {
    definition: {
      name: "echo",
      version: "1",
      effect: "read",
      capability: "test",
      description: "echo",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    async execute(ctx, input) {
      return {
        ok: true,
        data: { echoed: input.text },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

/** A signal-aware tool that never resolves on its own — only ctx.signal ends it. */
function makeSlowTool(): AiTool<Record<string, never>, { done: boolean }> {
  return {
    definition: {
      name: "slow",
      version: "1",
      effect: "read",
      capability: "test",
      description: "slow",
      inputSchema: { type: "object", properties: {} },
      timeoutMs: 20,
    },
    async execute(ctx) {
      await new Promise<void>((resolve, reject) => {
        const t = setTimeout(resolve, 5000);
        ctx.signal?.addEventListener(
          "abort",
          () => {
            clearTimeout(t);
            reject(new Error("aborted"));
          },
          { once: true },
        );
      });
      return {
        ok: true,
        data: { done: true },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

describe("runChat", () => {
  it("streams text-only response and completes", async () => {
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "Hello" }] }]);
    const messages: AiChatMessage[] = [{ role: "user", content: "hi" }];
    const events = await collect({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages,
      limits: resolveToolLimits({ preference: "small" }),
    });
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
    expect(events.filter((e) => e.type === "run.message.delta").length).toBe(1);
    const last = messages.at(-1);
    expect(last?.role).toBe("assistant");
    expect(last?.content).toBe("Hello");
  });

  it("executes a tool call and re-invokes the model", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "echo",
            argumentsJson: '{"text":"ok"}',
          },
        ],
      },
      { steps: [{ kind: "text", content: "Done." }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeEchoTool() as unknown as AiTool);
    const messages: AiChatMessage[] = [{ role: "user", content: "go" }];
    const events = await collect({
      client,
      registry,
      model: "m",
      messages,
      limits: resolveToolLimits({ preference: "small" }),
    });
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(true);
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
    // assistant tool_call message + tool result + final assistant
    const roles = messages.map((m) => m.role);
    expect(roles).toEqual(["user", "assistant", "tool", "assistant"]);
  });

  it("fails gracefully when tool not registered", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "missing",
            argumentsJson: "{}",
          },
        ],
      },
      { steps: [{ kind: "text", content: "Sorry." }] },
    ]);
    const events = await collect({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages: [{ role: "user", content: "go" }],
      limits: resolveToolLimits({ preference: "small" }),
    });
    expect(events.some((e) => e.type === "run.tool.failed")).toBe(true);
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
  });

  it("emits a truncated warning and accurate finishReason on finish_reason=length", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [{ kind: "text", content: "partial answer" }],
        finishReason: "length",
      },
    ]);
    const events = await collect({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages: [{ role: "user", content: "hi" }],
      limits: resolveToolLimits({ preference: "small" }),
    });
    const warnings = events.filter((e) => e.type === "run.warning");
    expect(
      warnings.some(
        (w) => (w as { data: { code: string } }).data.code === "truncated",
      ),
    ).toBe(true);
    const completed = events.find((e) => e.type === "run.completed");
    expect(
      (completed as { data: { finishReason?: string } }).data.finishReason,
    ).toBe("length");
  });

  it("defaults finishReason to stop when the provider omits it", async () => {
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "done" }] }]);
    const events = await collect({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages: [{ role: "user", content: "hi" }],
      limits: resolveToolLimits({ preference: "small" }),
    });
    const completed = events.find((e) => e.type === "run.completed");
    expect(
      (completed as { data: { finishReason?: string } }).data.finishReason,
    ).toBe("stop");
    expect(events.some((e) => e.type === "run.warning")).toBe(false);
  });

  it("respects maxToolIterations", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "echo",
            argumentsJson: '{"text":"a"}',
          },
        ],
      },
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c2",
            name: "echo",
            argumentsJson: '{"text":"b"}',
          },
        ],
      },
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c3",
            name: "echo",
            argumentsJson: '{"text":"c"}',
          },
        ],
      },
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeEchoTool() as unknown as AiTool);
    const events = await collect({
      client,
      registry,
      model: "m",
      messages: [{ role: "user", content: "loop" }],
      limits: resolveToolLimits({ preference: "small" }),
      maxToolIterations: 2,
    });
    const warnings = events.filter((e) => e.type === "run.warning");
    expect(
      warnings.some(
        (w) => (w as { data: { code: string } }).data.code === "max_iterations",
      ),
    ).toBe(true);
  });

  it("aborts a tool that exceeds its timeoutMs and continues the run", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "slow",
            argumentsJson: "{}",
          },
        ],
      },
      { steps: [{ kind: "text", content: "handled" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeSlowTool() as unknown as AiTool);
    const events = await collect({
      client,
      registry,
      model: "m",
      messages: [{ role: "user", content: "go" }],
      limits: resolveToolLimits({ preference: "small" }),
    });
    const failed = events.find((e) => e.type === "run.tool.failed") as
      | { data: { errorMessage: string } }
      | undefined;
    expect(failed).toBeDefined();
    expect(failed?.data.errorMessage).toContain("timed out");
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
  });
});
