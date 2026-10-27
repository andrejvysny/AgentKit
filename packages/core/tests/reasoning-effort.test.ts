import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import { collect, harness, message, request } from "./responses-helpers.js";

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

const okStream = [
  { choices: [{ delta: { content: "ok" } }] },
  { choices: [{ delta: {}, finish_reason: "stop" }] },
];

async function run(
  fetchImpl: typeof fetch,
  extra: Record<string, unknown> = { reasoningEffort: "high" },
): Promise<AiRunEvent[]> {
  const client = new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost:9/v1",
    fetchImpl,
  });
  const out: AiRunEvent[] = [];
  for await (const e of client.streamChat({
    runId: "r",
    model: "m",
    messages: [{ role: "user", content: "hi" }],
    ...extra,
  }))
    out.push(e);
  return out;
}

describe("chat completions reasoning_effort", () => {
  it("sends reasoning_effort when requested and omits it otherwise", async () => {
    const bodies: Array<Record<string, unknown>> = [];
    const fetchImpl = (async (_u: string | URL, init?: RequestInit) => {
      bodies.push(JSON.parse(String(init?.body)));
      return sseResponse(okStream);
    }) as unknown as typeof fetch;
    await run(fetchImpl);
    await run(fetchImpl, {});
    expect(bodies[0]).toHaveProperty("reasoning_effort", "high");
    expect(bodies[1]).not.toHaveProperty("reasoning_effort");
  });

  it("drops it once on a 400 naming it and warns", async () => {
    const bodies: Array<Record<string, unknown>> = [];
    const fetchImpl = (async (_u: string | URL, init?: RequestInit) => {
      const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
      bodies.push(body);
      if ("reasoning_effort" in body)
        return new Response(
          JSON.stringify({
            error: { message: "Unknown parameter: 'reasoning_effort'." },
          }),
          { status: 400 },
        );
      return sseResponse(okStream);
    }) as unknown as typeof fetch;
    const events = await run(fetchImpl);
    expect(bodies).toHaveLength(2);
    expect(bodies[1]).not.toHaveProperty("reasoning_effort");
    expect(events.some((e) => e.type === "run.failed")).toBe(false);
    const warning = events.find(
      (e) =>
        e.type === "run.warning" &&
        e.data.code === "reasoning_effort_unsupported",
    );
    expect(warning?.type === "run.warning" && warning.data.message).toBe(
      "Provider rejected reasoning_effort=high; continuing without it.",
    );
  });

  it("does not retry a second time when the server still rejects", async () => {
    let calls = 0;
    const fetchImpl = (async () => {
      calls++;
      return new Response("reasoning_effort unsupported", { status: 400 });
    }) as unknown as typeof fetch;
    const events = await run(fetchImpl);
    expect(calls).toBe(2);
    expect(events.filter((e) => e.type === "run.failed")).toHaveLength(1);
  });

  it("parses cached and reasoning token details", async () => {
    const fetchImpl = (async () =>
      sseResponse([
        { choices: [{ delta: { content: "ok" } }] },
        {
          choices: [{ delta: {}, finish_reason: "stop" }],
          usage: {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_tokens_details: { cached_tokens: 7 },
            completion_tokens_details: { reasoning_tokens: 3 },
          },
        },
      ])) as unknown as typeof fetch;
    const events = await run(fetchImpl, {});
    const usage = events.find((e) => e.type === "run.usage");
    expect(usage?.type === "run.usage" && usage.data).toMatchObject({
      cachedPromptTokens: 7,
      reasoningTokens: 3,
    });
  });

  it("leaves the detail fields out when the server sends none", async () => {
    const fetchImpl = (async () =>
      sseResponse([
        {
          choices: [{ delta: { content: "ok" }, finish_reason: "stop" }],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        },
      ])) as unknown as typeof fetch;
    const events = await run(fetchImpl, {});
    const usage = events.find((e) => e.type === "run.usage");
    expect(usage?.type === "run.usage" && usage.data).not.toHaveProperty(
      "cachedPromptTokens",
    );
  });
});

describe("Responses reasoning effort and usage details", () => {
  it("sends reasoning.effort alongside encrypted content include", async () => {
    const { client, sent } = harness();
    await collect(client, { ...request, reasoningEffort: "low" });
    const body = sent[0]?.body as Record<string, unknown>;
    expect(body.reasoning).toEqual({ effort: "low" });
    expect(body.include).toEqual(["reasoning.encrypted_content"]);
  });

  it("omits reasoning when no effort is requested", async () => {
    const { client, sent } = harness();
    await collect(client);
    expect(sent[0]?.body).not.toHaveProperty("reasoning");
  });

  it("parses cached and reasoning token details", async () => {
    const { client } = harness([
      {
        type: "response.completed",
        response: {
          id: "response",
          status: "completed",
          output: [message("Hi")],
          usage: {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            input_tokens_details: { cached_tokens: 8 },
            output_tokens_details: { reasoning_tokens: 2 },
          },
        },
      },
    ]);
    const events = await collect(client);
    const usage = events.find((e) => e.type === "run.usage");
    expect(usage?.type === "run.usage" && usage.data).toMatchObject({
      cachedPromptTokens: 8,
      reasoningTokens: 2,
    });
  });
});
