import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { collectRun } from "./helpers.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiRunEvent } from "@agentkit/contracts";

function sseResponse(chunks: unknown[]): Response {
  const enc = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const c of chunks)
        controller.enqueue(enc.encode(`data: ${JSON.stringify(c)}\n\n`));
      controller.enqueue(enc.encode("data: [DONE]\n\n"));
      controller.close();
    },
  });
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

type UsageEvent = AiRunEvent & {
  data: {
    callId: string;
    attempt: number;
    step: number;
    model: string;
    promptTokens?: number;
    completionTokens?: number;
    totalTokens?: number;
    source: string;
    finalForCall: boolean;
  };
};

const usageEvents = (events: AiRunEvent[]): UsageEvent[] =>
  events.filter((e) => e.type === "run.usage") as UsageEvent[];

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

/** Serve a scripted sequence of streams, one per POST, recording each body. */
function scriptedFetch(streams: unknown[][]): {
  fetchImpl: typeof fetch;
  bodies: Array<Record<string, unknown>>;
} {
  const bodies: Array<Record<string, unknown>> = [];
  let index = 0;
  const fetchImpl = (async (_url: string | URL, init?: RequestInit) => {
    bodies.push(JSON.parse(String(init?.body)) as Record<string, unknown>);
    const chunks = streams[Math.min(index, streams.length - 1)] ?? [];
    index += 1;
    return sseResponse(chunks);
  }) as unknown as typeof fetch;
  return { fetchImpl, bodies };
}

function makeClient(fetchImpl: typeof fetch): OpenAiCompatibleClient {
  return new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost:9/v1",
    fetchImpl,
  });
}

async function streamDirect(
  client: OpenAiCompatibleClient,
): Promise<AiRunEvent[]> {
  const out: AiRunEvent[] = [];
  for await (const e of client.streamChat({
    runId: "r",
    model: "gpt-test",
    messages: [{ role: "user", content: "hi" }],
  }))
    out.push(e);
  return out;
}

describe("OpenAiCompatibleClient usage reporting", () => {
  it("requests include_usage and emits run.usage from the trailing usage chunk", async () => {
    const { fetchImpl, bodies } = scriptedFetch([
      [
        { choices: [{ delta: { content: "hello" } }] },
        { choices: [{ delta: {}, finish_reason: "stop" }] },
        // OpenAI sends usage on its own final chunk, with choices empty.
        {
          choices: [],
          usage: { prompt_tokens: 11, completion_tokens: 4, total_tokens: 15 },
        },
      ],
    ]);
    const events = await streamDirect(makeClient(fetchImpl));

    // Without stream_options the server never reports usage at all.
    expect(bodies[0]).toHaveProperty("stream", true);
    expect(bodies[0]!.stream_options).toEqual({ include_usage: true });

    const usage = usageEvents(events);
    expect(usage.length).toBe(1);
    expect(usage[0]!.data.promptTokens).toBe(11);
    expect(usage[0]!.data.completionTokens).toBe(4);
    expect(usage[0]!.data.totalTokens).toBe(15);
    expect(usage[0]!.data.source).toBe("stream");
    expect(usage[0]!.data.finalForCall).toBe(true);
    expect(usage[0]!.data.attempt).toBe(1);
    expect(usage[0]!.data.model).toBe("gpt-test");
    expect(usage[0]!.data.callId.length).toBeGreaterThan(0);
    expect(usage[0]!.runId).toBe("r");
  });

  it("reads usage that arrives alongside choices", async () => {
    const { fetchImpl } = scriptedFetch([
      [
        { choices: [{ delta: { content: "hi" } }] },
        {
          choices: [{ delta: {}, finish_reason: "stop" }],
          usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
        },
      ],
    ]);
    const usage = usageEvents(await streamDirect(makeClient(fetchImpl)));
    expect(usage.length).toBe(1);
    expect(usage[0]!.data.totalTokens).toBe(4);
  });

  it("emits nothing when the server reports no usage", async () => {
    const { fetchImpl } = scriptedFetch([
      [
        { choices: [{ delta: { content: "hi" } }] },
        { choices: [{ delta: {}, finish_reason: "stop" }] },
      ],
    ]);
    // A fabricated zero would read as "this call was free"; silence is honest.
    expect(usageEvents(await streamDirect(makeClient(fetchImpl))).length).toBe(
      0,
    );
  });

  it("does not ask for usage on the non-streaming capability probe", async () => {
    // stream_options is a streaming-only field; sending it on a plain POST is a
    // 400 on some servers.
    const probeBodies: Array<Record<string, unknown>> = [];
    const json = (obj: unknown): Response =>
      new Response(JSON.stringify(obj), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    const client = makeClient((async (url: string | URL, init?: RequestInit) => {
      if (String(url).endsWith("/models")) return json({ data: [] });
      probeBodies.push(
        JSON.parse(String(init?.body)) as Record<string, unknown>,
      );
      return json({ choices: [{ message: {} }] });
    }) as unknown as typeof fetch);
    await client.capabilities(undefined, "m");
    expect(probeBodies[0]).toHaveProperty("stream", false);
    expect(probeBodies[0]).not.toHaveProperty("stream_options");
  });
});

describe("runChat usage stamping", () => {
  it("reports one usage event per provider call, stamped with its step", async () => {
    const { fetchImpl } = scriptedFetch([
      // Call 1: the model asks for a tool.
      [
        {
          choices: [
            {
              delta: {
                tool_calls: [
                  {
                    index: 0,
                    id: "c1",
                    function: { name: "echo", arguments: '{"text":"a"}' },
                  },
                ],
              },
            },
          ],
        },
        { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
        {
          choices: [],
          usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 },
        },
      ],
      // Call 2: the model answers using the tool result.
      [
        { choices: [{ delta: { content: "done" } }] },
        { choices: [{ delta: {}, finish_reason: "stop" }] },
        {
          choices: [],
          usage: { prompt_tokens: 40, completion_tokens: 6, total_tokens: 46 },
        },
      ],
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeEchoTool() as unknown as AiTool);

    const { events, result } = await collectRun(
      runChat({
        client: makeClient(fetchImpl),
        registry,
        model: "gpt-test",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(2);

    const usage = usageEvents(events);
    expect(usage.length).toBe(2);
    // Each provider call is billed separately, so each gets its own id...
    expect(usage[0]!.data.callId).not.toBe(usage[1]!.data.callId);
    // ...and the loop attributes it to the round-trip it belongs to.
    expect(usage.map((u) => u.data.step)).toEqual([1, 2]);
    expect(usage.map((u) => u.data.totalTokens)).toEqual([15, 46]);
    // The second call's prompt is bigger — it carries the tool result back.
    expect(usage[1]!.data.promptTokens).toBeGreaterThan(
      usage[0]!.data.promptTokens!,
    );
    for (const u of usage) {
      expect(u.data.source).toBe("stream");
      expect(u.data.finalForCall).toBe(true);
      expect(u.data.attempt).toBe(1);
    }
  });
});
