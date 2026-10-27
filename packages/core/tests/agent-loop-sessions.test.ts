import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import { MockProviderClient, normalizeTrace } from "@agentkit/testing";
import type { AiChatRequest } from "../src/providers/client.js";
import { runAgentLoop } from "../src/runs/agent-loop.js";
import { runChat } from "../src/runs/run-loop.js";
import {
  type AgentSteeringInput,
  type AgentSteeringSource,
  humanRequestId,
} from "../src/runs/session-seams.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import type { AiTool, AiToolExecutionContext } from "../src/tools/tool.js";
import { collectRun } from "./helpers.js";

const limits = resolveToolLimits({ preference: "small" });
const userMessages: AiChatMessage[] = [{ role: "user", content: "go" }];

/** Records each provider request's messages as they were at call time. */
class SpyClient extends MockProviderClient {
  requests: AiChatMessage[][] = [];
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push([...input.messages]);
    return super.streamChat(input);
  }
}

function tool(
  name: string,
  onExecute?: (ctx: AiToolExecutionContext) => void | Promise<void>,
): AiTool {
  return {
    definition: {
      name,
      version: "1",
      effect: "read",
      capability: "test",
      description: name,
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      await onExecute?.(ctx);
      return {
        ok: true,
        data: { name },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

function registryOf(...tools: AiTool[]): AiToolRegistry {
  const registry = new AiToolRegistry();
  for (const t of tools) registry.register(t);
  return registry;
}

const call = (id: string, name: string) =>
  ({ kind: "tool_call", toolCallId: id, name, argumentsJson: "{}" }) as const;

function client(
  turns: Parameters<MockProviderClient["setScript"]>[0],
): SpyClient {
  const c = new SpyClient();
  c.setScript(turns);
  return c;
}

const text = (content: string) => ({
  steps: [{ kind: "text", content } as const],
});

function loop(
  c: SpyClient,
  tools: AiTool[],
  extra: Partial<Parameters<typeof runAgentLoop>[0]> = {},
) {
  return collectRun(
    runAgentLoop({
      client: c,
      registry: registryOf(...tools),
      model: "m",
      messages: userMessages,
      limits,
      runId: "run1",
      ...extra,
    }),
  );
}

const steer = (inputId: string, role = "user"): AgentSteeringInput => ({
  inputId,
  message: { role, content: `msg-${inputId}` } as AiChatMessage,
});

function source(
  responses: (step: number) => readonly AgentSteeringInput[] | Promise<never>,
  seen: number[] = [],
): AgentSteeringSource {
  return {
    async pending({ step }) {
      seen.push(step);
      return responses(step);
    },
  };
}

const types = (events: AiRunEvent[]) => events.map((e) => e.type);
const warnings = (events: AiRunEvent[]) =>
  events.flatMap((e) => (e.type === "run.warning" ? [e.data.code] : []));

describe("runAgentLoop steering", () => {
  it("places a steer between step 1's tools and step 2's request", async () => {
    const c = client([{ steps: [call("c1", "echo")] }, text("done")]);
    const { events, result } = await loop(c, [tool("echo")], {
      steering: source((s) => (s === 2 ? [steer("i1")] : [])),
    });
    const t = types(events);
    const applied = t.indexOf("run.input.applied");
    expect(t[applied - 1]).toBe("run.tool.succeeded");
    // Nothing of step 2 (its delta / completed message) precedes the placement.
    expect(
      t.slice(0, applied).filter((x) => x === "run.message.completed"),
    ).toHaveLength(1);
    expect(t.indexOf("run.message.completed", applied)).toBeGreaterThan(
      applied,
    );
    const ev = events[applied];
    expect(ev?.type === "run.input.applied" && ev.data).toEqual({
      inputId: "i1",
      kind: "steer",
      step: 2,
    });
    const last = c.requests[1]?.at(-1);
    expect(last).toEqual({ role: "user", content: "msg-i1" });
    expect(result.appendedMessages).toContainEqual(last as AiChatMessage);
  });

  it("places a duplicate inputId once", async () => {
    const c = client([
      { steps: [call("c1", "echo")] },
      { steps: [call("c2", "echo")] },
      text("done"),
    ]);
    const { events } = await loop(c, [tool("echo")], {
      steering: source(() => [steer("i1"), steer("i1")]),
    });
    expect(types(events).filter((x) => x === "run.input.applied")).toHaveLength(
      1,
    );
    expect(c.requests[2]?.filter((m) => m.content === "msg-i1")).toHaveLength(
      1,
    );
  });

  it("rejects a non-user role with steering_rejected", async () => {
    const c = client([{ steps: [call("c1", "echo")] }, text("done")]);
    const { events } = await loop(c, [tool("echo")], {
      steering: source(() => [steer("bad", "assistant")]),
    });
    expect(warnings(events)).toContain("steering_rejected");
    expect(types(events)).not.toContain("run.input.applied");
    expect(c.requests[1]?.some((m) => m.content === "msg-bad")).toBe(false);
  });

  it("polls only between tool steps, never on the last iteration", async () => {
    const seen: number[] = [];
    const c = client([
      { steps: [call("c1", "echo")] },
      { steps: [call("c2", "echo")] },
    ]);
    await loop(c, [tool("echo")], {
      maxToolIterations: 2,
      steering: source(() => [], seen),
    });
    expect(seen).toEqual([2]);

    const seen2: number[] = [];
    await loop(client([text("hi")]), [tool("echo")], {
      steering: source(() => [], seen2),
    });
    expect(seen2).toEqual([]);
  });

  it("a throwing source warns hook_failed and the run completes", async () => {
    const c = client([{ steps: [call("c1", "echo")] }, text("done")]);
    const { events, result } = await loop(c, [tool("echo")], {
      steering: source(() => Promise.reject(new Error("boom"))),
    });
    const w = events.find(
      (e) => e.type === "run.warning" && e.data.code === "hook_failed",
    );
    expect(w?.type === "run.warning" && w.data.message).toBe(
      "Steering source threw; no steering applied this step.",
    );
    expect(result.terminal).toBe("completed");
  });

  it("a hanging source warns hook_timeout and the run completes", async () => {
    const c = client([{ steps: [call("c1", "echo")] }, text("done")]);
    const { events, result } = await loop(c, [tool("echo")], {
      hookTimeoutMs: 20,
      steering: { pending: () => new Promise(() => {}) },
    });
    expect(warnings(events)).toContain("hook_timeout");
    expect(result.terminal).toBe("completed");
  });

  it("without steering the stream equals runChat's", async () => {
    const script = () => [{ steps: [call("c1", "echo")] }, text("done")];
    const a = await loop(client(script()), [tool("echo")]);
    const b = await collectRun(
      runChat({
        client: client(script()),
        registry: registryOf(tool("echo")),
        model: "m",
        messages: userMessages,
        limits,
        runId: "run1",
      }),
    );
    expect(normalizeTrace(a.events)).toEqual(normalizeTrace(b.events));
  });
});

describe("runAgentLoop suspension", () => {
  const ask = (prompt: unknown = "Pick one") =>
    tool("ask", (ctx) => ctx.suspend?.({ prompt } as { prompt: string }));
  const toolMsgIds = (msgs: readonly AiChatMessage[]) =>
    msgs.flatMap((m) => (m.role === "tool" ? [m.toolCallId] : []));

  it("suspends on the waiting call; other calls still run", async () => {
    const settled: string[] = [];
    const c = client([
      { steps: [call("c1", "echo"), call("c2", "ask")] },
      text("never"),
    ]);
    const { events, result } = await loop(c, [tool("echo"), ask()], {
      suspendable: true,
      hooks: [{ onSettled: (x) => void settled.push(x.terminal) }],
    });
    expect(result.terminal).toBe("suspended");
    expect(toolMsgIds(result.appendedMessages)).toEqual(["c1"]);
    const last = events.at(-1);
    expect(last?.type).toBe("run.waiting");
    expect(last?.type === "run.waiting" && last.data).toEqual({
      requestId: humanRequestId("run1", "c2"),
      toolCallId: "c2",
      toolName: "ask",
      step: 1,
      prompt: "Pick one",
    });
    expect(types(events)).not.toContain("run.completed");
    expect(c.callCount).toBe(1);
    expect(settled).toEqual(["suspended"]);
  });

  it("second suspender in a batch fails suspend_conflict", async () => {
    const c = client([
      { steps: [call("c1", "ask"), call("c2", "ask")] },
      text("never"),
    ]);
    const { events, result } = await loop(c, [ask()], { suspendable: true });
    const failed = events.find((e) => e.type === "run.tool.failed");
    expect(failed?.type === "run.tool.failed" && failed.data).toMatchObject({
      toolCallId: "c2",
      errorCode: "suspend_conflict",
    });
    const last = events.at(-1);
    expect(last?.type === "run.waiting" && last.data.toolCallId).toBe("c1");
    expect(result.terminal).toBe("suspended");
  });

  it("an empty prompt fails suspend_invalid and the run completes", async () => {
    const c = client([{ steps: [call("c1", "ask")] }, text("done")]);
    const { events, result } = await loop(c, [ask("  ")], {
      suspendable: true,
    });
    const failed = events.find((e) => e.type === "run.tool.failed");
    expect(failed?.type === "run.tool.failed" && failed.data.errorCode).toBe(
      "suspend_invalid",
    );
    expect(result.terminal).toBe("completed");
  });

  it("not suspendable: ctx has no suspend key", async () => {
    const keys: boolean[] = [];
    const probe = tool("probe", (ctx) => void keys.push("suspend" in ctx));
    await loop(client([{ steps: [call("c1", "probe")] }, text("d")]), [probe]);
    await loop(client([{ steps: [call("c1", "probe")] }, text("d")]), [probe], {
      suspendable: false,
    });
    await collectRun(
      runChat({
        client: client([{ steps: [call("c1", "probe")] }, text("d")]),
        registry: registryOf(probe),
        model: "m",
        messages: userMessages,
        limits,
      }),
    );
    expect(keys).toEqual([false, false, false]);
  });

  it("abort after suspend cancels and answers every tool_call_id", async () => {
    const ac = new AbortController();
    const c = client([
      { steps: [call("c1", "ask"), call("c2", "echo")] },
      text("never"),
    ]);
    const abortTool = tool("ask", (ctx) => {
      ctx.suspend?.({ prompt: "Pick one" });
      ac.abort();
    });
    const { events, result } = await loop(c, [abortTool, tool("echo")], {
      suspendable: true,
      signal: ac.signal,
    });
    expect(result.terminal).toBe("cancelled");
    expect(types(events).at(-1)).toBe("run.cancelled");
    expect(types(events)).not.toContain("run.waiting");
    expect(toolMsgIds(result.appendedMessages).sort()).toEqual(["c1", "c2"]);
  });

  it("a late suspend() after a tool timeout is ignored", async () => {
    const slow: AiTool = {
      ...tool("slow"),
      definition: { ...tool("slow").definition, timeoutMs: 10 },
      async execute(ctx) {
        await new Promise((r) => setTimeout(r, 40));
        ctx.suspend?.({ prompt: "late" });
        return {
          ok: true,
          data: {},
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    };
    const c = client([{ steps: [call("c1", "slow")] }, text("done")]);
    const { events, result } = await loop(c, [slow], { suspendable: true });
    await new Promise((r) => setTimeout(r, 60));
    expect(types(events)).not.toContain("run.waiting");
    expect(result.terminal).toBe("completed");
  });
});
