import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import {
  CompletedOnlyProviderClient,
  MockProviderClient,
  normalizeTrace,
} from "@agentkit/testing";
import { messageContentToText } from "../src/messages/content.js";
import { runAgentLoop } from "../src/runs/agent-loop.js";
import type { AgentLoopHooks } from "../src/runs/loop-hooks.js";
import { runChat } from "../src/runs/run-loop.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import type { AiTool } from "../src/tools/tool.js";
import { collectRun } from "./helpers.js";

const limits = resolveToolLimits({ preference: "small" });

function makeTool(
  seen: unknown[] = [],
  name = "echo",
): AiTool<{ text: string }, unknown> {
  return {
    definition: {
      name,
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
      seen.push(input);
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

function registryOf(tool: AiTool<{ text: string }, unknown>): AiToolRegistry {
  const registry = new AiToolRegistry();
  registry.register(tool as unknown as AiTool);
  return registry;
}

const userMessages: AiChatMessage[] = [{ role: "user", content: "go" }];

/** Two tool calls then a final answer, on the streaming mock. */
function mockTwoCalls(): MockProviderClient {
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
      ],
    },
    { steps: [{ kind: "text", content: "done" }] },
  ]);
  return client;
}

function completedTwoCalls(): CompletedOnlyProviderClient {
  const client = new CompletedOnlyProviderClient();
  client.setScript([
    {
      toolCalls: [
        { id: "c1", name: "echo", argumentsJson: '{"text":"a"}' },
        { id: "c2", name: "echo", argumentsJson: '{"text":"b"}' },
      ],
    },
    { content: "done" },
  ]);
  return client;
}

function recorder(id: string, log: string[]): AgentLoopHooks {
  return {
    onRunStart: () => void log.push(`${id}:start`),
    onStepStart: (c) => void log.push(`${id}:step${c.step}`),
    onModelResponse: (c) => void log.push(`${id}:model${c.step}`),
    beforeToolCall: (c) => {
      log.push(`${id}:before:${c.toolCallId}`);
      return undefined;
    },
    onToolResult: (c) => void log.push(`${id}:result:${c.toolCallId}`),
    onSettled: (c) => void log.push(`${id}:settled:${c.terminal}`),
  };
}

function envelopeData(message: AiChatMessage): Record<string, unknown> {
  return (
    JSON.parse(messageContentToText(message.content)) as {
      data: Record<string, unknown>;
    }
  ).data;
}

function run(
  client: MockProviderClient | CompletedOnlyProviderClient,
  extra: Partial<Parameters<typeof runAgentLoop>[0]> & {
    tool?: AiTool<{ text: string }, unknown>;
  } = {},
) {
  const { tool, ...rest } = extra;
  return collectRun(
    runAgentLoop({
      client,
      registry: registryOf(tool ?? makeTool()),
      model: "m",
      messages: userMessages,
      limits,
      ...rest,
    }),
  );
}

const contiguous = (events: AiRunEvent[]) =>
  events.every((e, i) => e.seq === (events[0]?.seq ?? 0) + i);

describe("runAgentLoop hooks", () => {
  for (const [label, make] of [
    ["MockProviderClient", mockTwoCalls],
    ["CompletedOnlyProviderClient", completedTwoCalls],
  ] as const) {
    it(`fires all six points in order across two hooks (${label})`, async () => {
      const log: string[] = [];
      const { result } = await run(make(), {
        hooks: [recorder("a", log), recorder("b", log)],
      });
      expect(result.terminal).toBe("completed");
      expect(log).toEqual([
        "a:start",
        "b:start",
        "a:step1",
        "b:step1",
        "a:model1",
        "b:model1",
        "a:before:c1",
        "b:before:c1",
        "a:result:c1",
        "b:result:c1",
        "a:before:c2",
        "b:before:c2",
        "a:result:c2",
        "b:result:c2",
        "a:step2",
        "b:step2",
        "a:model2",
        "b:model2",
        "a:settled:completed",
        "b:settled:completed",
      ]);
    });
  }

  it("an observer that throws becomes one hook_failed warning, text withheld", async () => {
    const { events, result } = await run(mockTwoCalls(), {
      hooks: [
        {},
        {
          onStepStart: (c) => {
            if (c.step === 1) throw new Error("SECRET-STACK");
          },
        },
      ],
    });
    expect(result.terminal).toBe("completed");
    const warnings = events.filter(
      (e) => e.type === "run.warning" && e.data.code === "hook_failed",
    );
    expect(warnings).toHaveLength(1);
    const message = (warnings[0] as { data: { message: string } }).data.message;
    expect(message).toContain("onStepStart[1]");
    expect(message).not.toContain("SECRET-STACK");
    expect(contiguous(events)).toBe(true);
  });

  it("an observer that hangs gives hook_timeout and the run completes", async () => {
    const { events, result } = await run(mockTwoCalls(), {
      hookTimeoutMs: 20,
      hooks: [{ onRunStart: () => new Promise<void>(() => {}) }],
    });
    expect(result.terminal).toBe("completed");
    expect(
      events.filter(
        (e) => e.type === "run.warning" && e.data.code === "hook_timeout",
      ),
    ).toHaveLength(1);
    expect(contiguous(events)).toBe(true);
  });

  const gates: Array<[string, AgentLoopHooks["beforeToolCall"]]> = [
    ["deny", () => ({ action: "deny", reason: "nope" })],
    [
      "throw",
      () => {
        throw new Error("SECRET-STACK");
      },
    ],
    ["hang", () => new Promise(() => {})],
    // Untyped JS can return anything; only an explicit allow passes.
    [
      "malformed action",
      () => ({ action: "maybe" }) as unknown as { action: "allow" },
    ],
    ["non-object", () => "allow" as unknown as { action: "allow" }],
    [
      "deny without reason",
      () => ({ action: "deny" }) as unknown as { action: "allow" },
    ],
  ];
  for (const [label, beforeToolCall] of gates) {
    it(`beforeToolCall ${label}: tool never runs, guard envelope, run goes on`, async () => {
      const seen: unknown[] = [];
      const { events, result } = await run(mockTwoCalls(), {
        tool: makeTool(seen),
        hookTimeoutMs: 20,
        hooks: [{ beforeToolCall }],
      });
      expect(seen).toHaveLength(0);
      expect(result.terminal).toBe("completed");
      expect(result.iterations).toBe(2);
      const failed = events.filter((e) => e.type === "run.tool.failed");
      expect(failed).toHaveLength(2);
      for (const e of failed) {
        expect((e as { data: { errorCode?: string } }).data.errorCode).toBe(
          "tool_hook_denied",
        );
      }
      const tools = result.appendedMessages.filter((m) => m.role === "tool");
      expect(tools.map((m) => m.toolCallId)).toEqual(["c1", "c2"]);
      for (const m of tools) {
        const data = envelopeData(m);
        expect(data.phase).toBe("guard");
        expect(data.retryable).toBe(false);
        expect(JSON.stringify(m.content)).not.toContain("SECRET-STACK");
      }
      if (label === "deny")
        expect(messageContentToText(tools[0]?.content ?? "")).toContain("nope");
    });
  }

  it("a deny short-circuits later hooks", async () => {
    const second: string[] = [];
    await run(mockTwoCalls(), {
      hooks: [
        { beforeToolCall: () => ({ action: "deny", reason: "no" }) },
        { beforeToolCall: (c) => void second.push(c.toolCallId) },
      ],
    });
    expect(second).toEqual([]);
  });

  it("hook cannot mutate args; tool gets the original", async () => {
    const seen: unknown[] = [];
    let mutated = true;
    await run(mockTwoCalls(), {
      tool: makeTool(seen),
      hooks: [
        {
          beforeToolCall: (c) => {
            const args = c.args as { text: string };
            try {
              args.text = "evil";
            } catch {
              // strict mode: frozen
            }
            mutated = args.text === "evil";
            return undefined;
          },
        },
      ],
    });
    expect(mutated).toBe(false);
    expect(seen).toEqual([{ text: "a" }, { text: "b" }]);
  });

  it("abort inside beforeToolCall cancels: no execute, remaining balanced", async () => {
    const seen: unknown[] = [];
    const ac = new AbortController();
    const { events, result } = await run(mockTwoCalls(), {
      tool: makeTool(seen),
      signal: ac.signal,
      hooks: [{ beforeToolCall: () => void ac.abort() }],
    });
    expect(seen).toHaveLength(0);
    expect(result.terminal).toBe("cancelled");
    expect(events.at(-1)?.type).toBe("run.cancelled");
    const tools = result.appendedMessages.filter((m) => m.role === "tool");
    expect(tools.map((m) => m.toolCallId)).toEqual(["c1", "c2"]);
  });

  describe("onSettled", () => {
    const settle = async (
      client: MockProviderClient,
      extra: Parameters<typeof run>[1] = {},
    ) => {
      const settled: Array<[string, number]> = [];
      const { result } = await run(client, {
        ...extra,
        hooks: [
          { onSettled: (c) => void settled.push([c.terminal, c.iterations]) },
        ],
      });
      return { settled, result };
    };

    it("completed", async () => {
      const { settled } = await settle(mockTwoCalls());
      expect(settled).toEqual([["completed", 2]]);
    });

    it("failed (provider throw)", async () => {
      const client = new MockProviderClient();
      client.streamChat = () => {
        throw new Error("boom");
      };
      const { settled, result } = await settle(client);
      expect(result.terminal).toBe("failed");
      expect(settled.map((s) => s[0])).toEqual(["failed"]);
    });

    it("cancelled (pre-aborted)", async () => {
      const ac = new AbortController();
      ac.abort();
      const { settled } = await settle(mockTwoCalls(), { signal: ac.signal });
      expect(settled.map((s) => s[0])).toEqual(["cancelled"]);
    });

    it("budget exhausted", async () => {
      const { settled, result } = await settle(mockTwoCalls(), {
        budgets: { providerRequests: 1 },
      });
      expect(result.terminal).toBe("failed");
      expect(settled.map((s) => s[0])).toEqual(["failed"]);
    });
  });

  it("hook contexts never carry continuation or run content", async () => {
    const recorded: unknown[] = [];
    const push = (c: unknown) => void recorded.push(c);
    await run(mockTwoCalls(), {
      hooks: [
        {
          onRunStart: push,
          onStepStart: push,
          onModelResponse: push,
          beforeToolCall: (c) => void push(c),
          onToolResult: push,
          onSettled: push,
        },
      ],
    });
    expect(recorded.length).toBeGreaterThan(5);
    const keys = new Set<string>();
    const scan = (v: unknown): void => {
      if (v && typeof v === "object") {
        for (const [k, child] of Object.entries(v)) {
          keys.add(k);
          scan(child);
        }
      }
    };
    recorded.forEach(scan);
    for (const banned of ["continuation", "continuationScope", "runContent"])
      expect(keys.has(banned)).toBe(false);
  });

  it("hooks: [] is byte-identical to runChat", async () => {
    const viaLoop = await run(mockTwoCalls(), { runId: "r", hooks: [] });
    const viaChat = await collectRun(
      runChat({
        client: mockTwoCalls(),
        registry: registryOf(makeTool()),
        model: "m",
        messages: userMessages,
        limits,
        runId: "r",
      }),
    );
    expect(normalizeTrace(viaLoop.events)).toEqual(
      normalizeTrace(viaChat.events),
    );
  });
});
