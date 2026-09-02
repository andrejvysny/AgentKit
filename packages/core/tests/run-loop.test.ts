import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { MockProviderClient } from "@agentkit/testing";
import { collectRun } from "./helpers.js";
import type { AiTool, AiToolExecutionContext } from "../src/tools/tool.js";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";

/** Events only, for the cases that don't care about the return value. */
async function collect(
  input: Parameters<typeof runChat>[0],
): Promise<AiRunEvent[]> {
  return (await collectRun(runChat(input))).events;
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
    const { events, result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages,
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
    expect(events.filter((e) => e.type === "run.message.delta").length).toBe(1);
    const last = result.appendedMessages.at(-1);
    expect(last?.role).toBe("assistant");
    expect(last?.content).toBe("Hello");
    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(1);
    // The caller's array is untouched — the run hands back what it appended.
    expect(messages.length).toBe(1);
    expect(messages).toEqual([{ role: "user", content: "hi" }]);
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
    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages,
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(true);
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
    // assistant tool_call message + tool result + final assistant
    const roles = [...messages, ...result.appendedMessages].map((m) => m.role);
    expect(roles).toEqual(["user", "assistant", "tool", "assistant"]);
    // ...all of which live in the result, not in the caller's array.
    expect(messages.map((m) => m.role)).toEqual(["user"]);
    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(2);
  });

  it("hands a tool the run's chatId and scopeId, and omits what it wasn't given", async () => {
    const seen: AiToolExecutionContext[] = [];
    const spyTool: AiTool<{ text: string }, null> = {
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
      async execute(ctx) {
        seen.push(ctx);
        return {
          ok: true,
          data: null,
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    };
    const script = (): MockProviderClient => {
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
      return client;
    };
    const registry = new AiToolRegistry();
    registry.register(spyTool as unknown as AiTool);
    const base = {
      registry,
      model: "m",
      messages: [{ role: "user" as const, content: "go" }],
      limits: resolveToolLimits({ preference: "small" }),
    };

    await collect({
      ...base,
      client: script(),
      chatId: "chat-7",
      // The document two chats share — deliberately NOT the chat id, so a tool
      // that fell back to chatId would read the wrong namespace.
      scopeId: "doc-42",
    });
    expect(seen).toHaveLength(1);
    expect(seen[0]?.chatId).toBe("chat-7");
    expect(seen[0]?.scopeId).toBe("doc-42");

    // Both are optional: a caller that names neither hands the tool neither.
    await collect({ ...base, client: script() });
    expect(seen).toHaveLength(2);
    expect(seen[1]?.chatId).toBeUndefined();
    expect(seen[1]?.scopeId).toBeUndefined();
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
    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "loop" }],
        limits: resolveToolLimits({ preference: "small" }),
        maxToolIterations: 2,
      }),
    );
    const warnings = events.filter((e) => e.type === "run.warning");
    expect(
      warnings.some(
        (w) => (w as { data: { code: string } }).data.code === "max_iterations",
      ),
    ).toBe(true);
    // Exhaustion still ends on run.completed, so the reported terminal matches.
    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(2);
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

  it("re-keys duplicate tool_call_ids from ANY provider client", async () => {
    // The first-party client dedupes its own accumulators, but `AiProviderClient`
    // is an interface a host may implement: two calls under one id used to
    // produce two `role:"tool"` messages sharing a `tool_call_id` — one answer
    // for two calls, and colliding projections.
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "dup",
            name: "echo",
            argumentsJson: JSON.stringify({ text: "one" }),
          },
          {
            kind: "tool_call",
            toolCallId: "dup",
            name: "echo",
            argumentsJson: JSON.stringify({ text: "two" }),
          },
        ],
      },
      { steps: [{ kind: "text", content: "done" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeEchoTool() as unknown as AiTool);

    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    const warning = events.find(
      (e) =>
        e.type === "run.warning" && e.data.code === "duplicate_tool_call_id",
    );
    expect(warning).toBeDefined();
    const toolIds = result.appendedMessages
      .filter((m) => m.role === "tool")
      .map((m) => (m as { toolCallId: string }).toolCallId);
    expect(toolIds).toEqual(["dup", "dup#2"]);
    // The assistant message must list exactly the ids its answers use.
    const assistant = result.appendedMessages.find(
      (m) => m.role === "assistant",
    ) as { toolCalls?: { id: string }[] } | undefined;
    expect(assistant?.toolCalls?.map((tc) => tc.id)).toEqual(["dup", "dup#2"]);
  });
});
