import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import {
  AiToolRegistry,
  ExecutionBudget,
  runChat,
  resolveToolLimits,
  type AiTool,
} from "../src/index.js";

function fetchFor(handler: () => Promise<Response>): typeof fetch {
  return handler as unknown as typeof fetch;
}

function clientFor(frames: string[]): OpenAiCompatibleClient {
  return new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost",
    fetchImpl: fetchFor(
      async () =>
        new Response(frames.map((frame) => `data: ${frame}\n\n`).join(""), {
          headers: { "content-type": "text/event-stream" },
        }),
    ),
  });
}

const call = JSON.stringify({
  choices: [
    {
      delta: {
        content: "partial",
        tool_calls: [
          {
            index: 0,
            id: "call",
            function: { name: "write", arguments: "{}" },
          },
        ],
      },
    },
  ],
});
const finish = JSON.stringify({
  choices: [{ delta: {}, finish_reason: "tool_calls" }],
});

for (const [name, frames] of [
  ["transport EOF", [call]],
  ["malformed tool frame", [call, "{invalid", finish, "[DONE]"]],
] as const) {
  it(`never executes complete-looking tool arguments after ${name}`, async () => {
    let executions = 0;
    const tool: AiTool = {
      definition: {
        name: "write",
        version: "1",
        effect: "write",
        capability: "write",
        description: "Write",
        inputSchema: { type: "object" },
      },
      async execute(context) {
        executions++;
        return {
          ok: true,
          data: {},
          summary: "write",
          sources: [],
          warnings: [],
          truncated: false,
          limits: context.limits,
        };
      },
    };
    const registry = new AiToolRegistry();
    registry.register(tool);
    const events = [];
    for await (const event of runChat({
      client: clientFor([...frames]),
      registry,
      model: "m",
      messages: [{ role: "user", content: "write" }],
      limits: resolveToolLimits({ preference: "small" }),
    }))
      events.push(event);
    expect(executions).toBe(0);
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "incomplete_stream" },
    });
    expect(
      events.some(
        (event) =>
          event.type === "run.message.completed" ||
          event.type === "run.tool.requested",
      ),
    ).toBe(false);
    expect(
      events.some(
        (event) =>
          event.type === "run.message.delta" && event.data.delta === "partial",
      ),
    ).toBe(true);
  });
}

describe("OpenAI request reservation", () => {
  it("enforces a cumulative one-request budget across compatibility fallback", async () => {
    let requests = 0;
    const client = new OpenAiCompatibleClient({
      id: "test",
      kind: "openai-compatible",
      baseUrl: "http://localhost",
      fetchImpl: fetchFor(async () => {
        requests++;
        return new Response("max_tokens unsupported", { status: 400 });
      }),
    });
    const budget = new ExecutionBudget({ providerRequests: 1 });
    const events = [];
    try {
      for await (const event of runChat({
        client,
        executionBudget: budget,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        maxOutputTokens: 10,
        limits: resolveToolLimits({ preference: "small" }),
      }))
        events.push(event);
      expect(requests).toBe(1);
      expect(budget.snapshot().providerRequests).toBe(1);
      expect(events.at(-1)).toMatchObject({
        type: "run.failed",
        data: { errorCode: "execution_budget_exhausted" },
      });
    } finally {
      budget.dispose();
    }
  });

  it("reserves each HTTP dispatch, including the max_tokens compatibility fallback", async () => {
    let requests = 0;
    let reservations = 0;
    const client = new OpenAiCompatibleClient({
      id: "test",
      kind: "openai-compatible",
      baseUrl: "http://localhost",
      fetchImpl: fetchFor(async () => {
        requests++;
        if (requests === 1)
          return new Response(
            "max_tokens unsupported; use max_completion_tokens",
            { status: 400 },
          );
        return new Response(
          `data: ${JSON.stringify({ choices: [{ delta: { content: "ok" }, finish_reason: "stop" }] })}\n\ndata: [DONE]\n\n`,
          { headers: { "content-type": "text/event-stream" } },
        );
      }),
    });
    const events = [];
    for await (const event of client.streamChat({
      runId: "run",
      model: "m",
      messages: [],
      maxOutputTokens: 10,
      beforeRequest: async () => {
        reservations++;
      },
    }))
      events.push(event);
    expect(requests).toBe(2);
    expect(reservations).toBe(2);
    expect(events.at(-1)?.type).toBe("run.message.completed");
  });

  it("does not dispatch the compatibility fallback when its reservation is refused", async () => {
    let requests = 0;
    let reservations = 0;
    const client = new OpenAiCompatibleClient({
      id: "test",
      kind: "openai-compatible",
      baseUrl: "http://localhost",
      fetchImpl: fetchFor(async () => {
        requests++;
        return new Response("max_tokens unsupported", { status: 400 });
      }),
    });
    const events = [];
    for await (const event of client.streamChat({
      runId: "run",
      model: "m",
      messages: [],
      maxOutputTokens: 10,
      beforeRequest: async () => {
        if (++reservations === 2) throw new Error("request budget exhausted");
      },
    }))
      events.push(event);
    expect(requests).toBe(1);
    expect(reservations).toBe(2);
    expect(events.at(-1)?.type).toBe("run.failed");
  });
});
