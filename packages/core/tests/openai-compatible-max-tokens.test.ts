import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
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

function makeClient(fetchImpl: typeof fetch): OpenAiCompatibleClient {
  return new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost:9/v1",
    fetchImpl,
  });
}

async function streamWith(
  client: OpenAiCompatibleClient,
): Promise<AiRunEvent[]> {
  const out: AiRunEvent[] = [];
  for await (const e of client.streamChat({
    runId: "r",
    model: "m",
    messages: [{ role: "user", content: "hi" }],
    maxOutputTokens: 64,
  }))
    out.push(e);
  return out;
}

describe("OpenAiCompatibleClient max_tokens fallback", () => {
  it("retries with max_completion_tokens when the server rejects max_tokens", async () => {
    const bodies: Array<Record<string, unknown>> = [];
    const fetchImpl = (async (_url: string | URL, init?: RequestInit) => {
      const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
      bodies.push(body);
      if ("max_tokens" in body) {
        return new Response(
          JSON.stringify({
            error: { message: "Unsupported parameter: 'max_tokens'." },
          }),
          { status: 400, headers: { "content-type": "application/json" } },
        );
      }
      return sseResponse([
        { choices: [{ delta: { content: "ok" } }] },
        { choices: [{ delta: {}, finish_reason: "stop" }] },
      ]);
    }) as unknown as typeof fetch;

    const events = await streamWith(makeClient(fetchImpl));
    // First attempt used max_tokens, retry translated to max_completion_tokens.
    expect(bodies.length).toBe(2);
    expect(bodies[0]).toHaveProperty("max_tokens", 64);
    expect(bodies[1]).not.toHaveProperty("max_tokens");
    expect(bodies[1]).toHaveProperty("max_completion_tokens", 64);
    // The retry succeeded and we did NOT emit run.failed.
    expect(events.some((e) => e.type === "run.failed")).toBe(false);
    expect(events.some((e) => e.type === "run.message.completed")).toBe(true);
  });

  it("does not retry on an unrelated 400 and emits a single run.failed", async () => {
    let calls = 0;
    const fetchImpl = (async () => {
      calls++;
      return new Response(
        JSON.stringify({ error: { message: "model not found" } }),
        { status: 400, headers: { "content-type": "application/json" } },
      );
    }) as unknown as typeof fetch;

    const events = await streamWith(makeClient(fetchImpl));
    expect(calls).toBe(1);
    expect(events.filter((e) => e.type === "run.failed").length).toBe(1);
  });
});

describe("OpenAiCompatibleClient finish_reason=tool_calls guard", () => {
  it("warns when finish_reason=tool_calls but no tool call is reconstructable", async () => {
    const fetchImpl = (async () =>
      sseResponse([
        { choices: [{ delta: { content: "" } }] },
        { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
      ])) as unknown as typeof fetch;
    const events: AiRunEvent[] = [];
    for await (const e of makeClient(fetchImpl).streamChat({
      runId: "r",
      model: "m",
      messages: [{ role: "user", content: "hi" }],
    }))
      events.push(e);
    expect(
      events.some(
        (e) =>
          e.type === "run.warning" &&
          (e as { data: { code: string } }).data.code ===
            "tool_call_unparseable",
      ),
    ).toBe(true);
  });
});
