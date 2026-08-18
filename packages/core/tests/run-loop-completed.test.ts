import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { MockProviderClient } from "./mocks/mock-provider.js";
import { CompletedOnlyProviderClient } from "./mocks/mock-completed-provider.js";
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

/** A tool that always reports failure via its OWN ok flag (without throwing). */
function makeFailingTool(): AiTool<{ text: string }, { reason: string }> {
  return {
    definition: {
      name: "fails",
      version: "1",
      effect: "write",
      capability: "test",
      description: "always fails",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    async execute(ctx, _input) {
      return {
        ok: false,
        data: { reason: "boom" },
        sources: [],
        warnings: ["the apply failed"],
        truncated: false,
        limits: ctx.limits,
        summary: "Apply failed: bad pin",
        status: "partial",
      };
    },
  };
}

describe("runChat — completed-only providers", () => {
  it("captures assistant content from a completed-only {content} turn", async () => {
    const client = new CompletedOnlyProviderClient();
    client.setScript([{ content: "Final answer", finishReason: "stop" }]);
    const messages: AiChatMessage[] = [{ role: "user", content: "hi" }];
    const events = await collect({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages,
      limits: resolveToolLimits({ preference: "small" }),
    });
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
    const last = messages.at(-1);
    expect(last?.role).toBe("assistant");
    expect(last?.content).toBe("Final answer");
  });

  it("executes a tool from completed-only {toolCalls} (no deltas/requested)", async () => {
    const client = new CompletedOnlyProviderClient();
    client.setScript([
      {
        toolCalls: [{ id: "c1", name: "echo", argumentsJson: '{"text":"ok"}' }],
        finishReason: "tool_calls",
      },
      { content: "Done.", finishReason: "stop" },
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
    expect(messages.map((m) => m.role)).toEqual([
      "user",
      "assistant",
      "tool",
      "assistant",
    ]);
  });
});

describe("runChat — tool-own ok and envelopes", () => {
  it("emits run.tool.failed when a tool returns { ok: false } (no throw)", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "fails",
            argumentsJson: '{"text":"x"}',
          },
        ],
      },
      { steps: [{ kind: "text", content: "ack" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeFailingTool() as unknown as AiTool);
    const messages: AiChatMessage[] = [{ role: "user", content: "go" }];
    const events = await collect({
      client,
      registry,
      model: "m",
      messages,
      limits: resolveToolLimits({ preference: "small" }),
    });
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(false);
    const failed = events.find((e) => e.type === "run.tool.failed") as
      | (AiRunEvent & {
          data: {
            errorMessage: string;
            errorCode?: string;
            status?: string;
            modelResultJson?: string;
          };
        })
      | undefined;
    expect(failed).toBeDefined();
    expect(failed!.data.errorMessage).toBe("Apply failed: bad pin");
    // #3: the failed EVENT carries the balanced envelope (status:"partial" +
    // modelData) so a consumer (run-service) can persist/replay it faithfully
    // instead of synthesizing a generic error.
    expect(failed!.data.status).toBe("partial");
    expect(failed!.data.modelResultJson).toBeDefined();
    const evtEnv = JSON.parse(failed!.data.modelResultJson!) as {
      status: string;
    };
    expect(evtEnv.status).toBe("partial");
    // Error envelope fed back to the model on the tool message.
    const toolMsg = messages.find((m) => m.role === "tool");
    expect(toolMsg).toBeDefined();
    const env = JSON.parse(toolMsg!.content) as {
      ok: boolean;
      status: string;
      warnings: string[];
    };
    expect(env.ok).toBe(false);
    // A tool reporting ok:false WITH status:"partial" preserves "partial" in the
    // balanced envelope (F5b) — a partial apply is not a hard error.
    expect(env.status).toBe("partial");
    expect(env.warnings).toEqual(["the apply failed"]);
  });

  it("feeds a balanced envelope (modelData + warnings/truncated/status) on success", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "c1",
            name: "slim",
            argumentsJson: "{}",
          },
        ],
      },
      { steps: [{ kind: "text", content: "ok" }] },
    ]);
    const slimTool: AiTool = {
      definition: {
        name: "slim",
        version: "1",
        effect: "read",
        capability: "test",
        description: "returns slim modelData",
        inputSchema: { type: "object", properties: {} },
      },
      async execute(ctx) {
        return {
          ok: true,
          data: { huge: "x".repeat(1000) },
          modelData: { count: 3 },
          summary: "3 items",
          status: "ok",
          sources: [],
          warnings: ["heads up"],
          truncated: true,
          limits: ctx.limits,
        };
      },
    };
    const registry = new AiToolRegistry();
    registry.register(slimTool);
    const messages: AiChatMessage[] = [{ role: "user", content: "go" }];
    const events = await collect({
      client,
      registry,
      model: "m",
      messages,
      limits: resolveToolLimits({ preference: "small" }),
    });
    const ok = events.find((e) => e.type === "run.tool.succeeded") as
      | (AiRunEvent & {
          data: {
            resultJson: string;
            modelResultJson?: string;
            summary?: string;
            status?: string;
          };
        })
      | undefined;
    expect(ok).toBeDefined();
    expect(ok!.data.summary).toBe("3 items");
    expect(ok!.data.status).toBe("ok");
    // Full payload preserved for UI in resultJson.
    expect(JSON.parse(ok!.data.resultJson).huge.length).toBe(1000);
    // Model sees the slim envelope: modelData, not the full data.
    const env = JSON.parse(ok!.data.modelResultJson!) as {
      data: { count: number };
      warnings: string[];
      truncated: boolean;
    };
    expect(env.data).toEqual({ count: 3 });
    expect(env.warnings).toEqual(["heads up"]);
    expect(env.truncated).toBe(true);
    // The tool message fed to the model is the slim envelope, not the full data.
    const toolMsg = messages.find((m) => m.role === "tool");
    expect(toolMsg!.content).not.toContain("xxxx");
  });
});

describe("runChat — terminal events for every requested/capped call", () => {
  it("gives each over-cap call a terminal failed event + balanced messages", async () => {
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
          {
            kind: "tool_call",
            toolCallId: "c2",
            name: "echo",
            argumentsJson: '{"text":"b"}',
          },
          {
            kind: "tool_call",
            toolCallId: "c3",
            name: "echo",
            argumentsJson: '{"text":"c"}',
          },
        ],
      },
      { steps: [{ kind: "text", content: "done" }] },
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
      maxToolCallsPerIteration: 1,
    });
    // One success (c1) + two capped failures (c2, c3).
    expect(events.filter((e) => e.type === "run.tool.succeeded").length).toBe(
      1,
    );
    const capped = events.filter(
      (e) =>
        e.type === "run.tool.failed" &&
        (e as { data: { errorCode?: string } }).data.errorCode ===
          "tool_call_cap",
    );
    expect(capped.length).toBe(2);
    // F1: the assistant message lists EVERY tool call it will answer (all 3),
    // not just the executed subset — otherwise capped calls become orphan
    // tool_call_ids that Chat Completions rejects.
    const assistantMsg = messages.find(
      (m) => m.role === "assistant" && m.toolCalls && m.toolCalls.length > 0,
    );
    expect(assistantMsg?.toolCalls?.map((tc) => tc.id)).toEqual([
      "c1",
      "c2",
      "c3",
    ]);
    // Every assistant tool_call_id has a matching tool message (no dangling state).
    const toolMsgIds = messages
      .filter((m) => m.role === "tool")
      .map((m) => m.toolCallId);
    expect(new Set(toolMsgIds)).toEqual(new Set(["c1", "c2", "c3"]));
    // The capped (skipped) tool messages carry the balanced error envelope (F10).
    const cappedMsg = messages.find(
      (m) => m.role === "tool" && m.toolCallId === "c2",
    );
    const cappedEnv = JSON.parse(cappedMsg!.content) as {
      ok: boolean;
      status: string;
      data: { errorCode: string };
    };
    expect(cappedEnv.ok).toBe(false);
    expect(cappedEnv.status).toBe("error");
    expect(cappedEnv.data.errorCode).toBe("tool_call_cap");
  });
});

describe("runChat — finish_reason tool_calls with no usable call", () => {
  it("warns instead of returning an empty success", async () => {
    const client = new CompletedOnlyProviderClient();
    // finish_reason=tool_calls but the provider emitted no tool call.
    client.setScript([{ content: "", finishReason: "tool_calls" }]);
    const events = await collect({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages: [{ role: "user", content: "go" }],
      limits: resolveToolLimits({ preference: "small" }),
    });
    expect(
      events.some(
        (e) =>
          e.type === "run.warning" &&
          (e as { data: { code: string } }).data.code === "tool_call_cap",
      ),
    ).toBe(true);
    expect(events.find((e) => e.type === "run.completed")).toBeDefined();
  });
});
