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

function sseRawResponse(dataLines: string[]): Response {
  const enc = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      for (const line of dataLines)
        controller.enqueue(enc.encode(`data: ${line}\n\n`));
      controller.enqueue(enc.encode("data: [DONE]\n\n"));
      controller.close();
    },
  });
  return new Response(body, {
    status: 200,
    headers: { "content-type": "text/event-stream" },
  });
}

function jsonResponse(obj: unknown): Response {
  return new Response(JSON.stringify(obj), {
    status: 200,
    headers: { "content-type": "application/json" },
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

async function collectStream(
  client: OpenAiCompatibleClient,
): Promise<AiRunEvent[]> {
  const out: AiRunEvent[] = [];
  for await (const e of client.streamChat({
    runId: "r",
    model: "m",
    messages: [{ role: "user", content: "hi" }],
  }))
    out.push(e);
  return out;
}

describe("OpenAiCompatibleClient.streamChat", () => {
  it("captures reasoning_content into the completed event without merging into content", async () => {
    const client = makeClient((async () =>
      sseResponse([
        { choices: [{ delta: { reasoning_content: "thinking" } }] },
        { choices: [{ delta: { reasoning_content: " harder" } }] },
        { choices: [{ delta: {}, finish_reason: "stop" }] },
      ])) as unknown as typeof fetch);
    const events = await collectStream(client);
    const completed = events.find((e) => e.type === "run.message.completed") as
      | (AiRunEvent & {
          data: {
            content: string;
            reasoningContent?: string;
            finishReason?: string;
          };
        })
      | undefined;
    expect(completed).toBeDefined();
    expect(completed!.data.content).toBe("");
    expect(completed!.data.reasoningContent).toBe("thinking harder");
    expect(completed!.data.finishReason).toBe("stop");
    // Reasoning is NOT streamed as visible deltas.
    expect(events.filter((e) => e.type === "run.message.delta").length).toBe(0);
  });

  it("keeps content and reasoning separate", async () => {
    const client = makeClient((async () =>
      sseResponse([
        { choices: [{ delta: { reasoning_content: "cot" } }] },
        { choices: [{ delta: { content: "answer" } }] },
        { choices: [{ delta: {}, finish_reason: "stop" }] },
      ])) as unknown as typeof fetch);
    const events = await collectStream(client);
    const completed = events.find((e) => e.type === "run.message.completed") as
      | (AiRunEvent & {
          data: { content: string; reasoningContent?: string };
        })
      | undefined;
    expect(completed!.data.content).toBe("answer");
    expect(completed!.data.reasoningContent).toBe("cot");
  });

  it("warns when malformed SSE lines drop the entire answer", async () => {
    const client = makeClient((async () =>
      sseRawResponse([
        "{not valid json",
        "{also bad",
      ])) as unknown as typeof fetch);
    const events = await collectStream(client);
    const warning = events.find(
      (e) =>
        e.type === "run.warning" &&
        (e as { data: { code: string } }).data.code === "sse_parse",
    );
    expect(warning).toBeDefined();
  });
});

describe("OpenAiCompatibleClient extraHeaders", () => {
  it("merges extraHeaders into requests but cannot override authorization", async () => {
    let seen: Headers | undefined;
    const client = new OpenAiCompatibleClient({
      id: "gateway",
      kind: "host-gateway",
      baseUrl: "http://localhost:9/v1/llm",
      apiKey: "real-bearer",
      extraHeaders: {
        "x-workspace-id": "ws_1",
        "x-run-id": "run_1",
        authorization: "Bearer SPOOFED",
        accept: "text/spoofed",
      },
      fetchImpl: (async (_url: string, init?: RequestInit) => {
        seen = new Headers(init?.headers);
        return sseResponse([{ choices: [{ delta: {}, finish_reason: "stop" }] }]);
      }) as unknown as typeof fetch,
    });
    for await (const _e of client.streamChat({
      runId: "r",
      model: "m",
      messages: [{ role: "user", content: "hi" }],
    }));
    expect(seen?.get("x-workspace-id")).toBe("ws_1");
    expect(seen?.get("x-run-id")).toBe("run_1");
    // The built-in headers win — extraHeaders cannot spoof auth or accept.
    expect(seen?.get("authorization")).toBe("Bearer real-bearer");
    expect(seen?.get("accept")).toBe("application/json");
  });

  it("fromConfig carries extraHeaders through to the outgoing request", async () => {
    let seen: Headers | undefined;
    const fetchImpl = (async (_url: string, init?: RequestInit) => {
      seen = new Headers(init?.headers);
      return sseResponse([{ choices: [{ delta: {}, finish_reason: "stop" }] }]);
    }) as unknown as typeof fetch;
    const client = OpenAiCompatibleClient.fromConfig(
      {
        id: "cfg",
        label: "Configured",
        kind: "openai-compatible",
        baseUrl: "http://localhost:9/v1",
        apiKey: "k",
        defaultModel: "m",
        enabled: true,
        extraHeaders: { "x-workspace-id": "ws_1", "x-run-id": "run_1" },
      },
      fetchImpl,
    );
    for await (const _e of client.streamChat({
      runId: "r",
      model: "m",
      messages: [{ role: "user", content: "hi" }],
    }));
    expect(seen?.get("x-workspace-id")).toBe("ws_1");
    expect(seen?.get("x-run-id")).toBe("run_1");
    expect(seen?.get("authorization")).toBe("Bearer k");
  });
});

describe("OpenAiCompatibleClient app attribution headers", () => {
  async function headersFor(
    options: Partial<ConstructorParameters<typeof OpenAiCompatibleClient>[0]>,
  ): Promise<Headers | undefined> {
    let seen: Headers | undefined;
    const client = new OpenAiCompatibleClient({
      id: "test",
      kind: "openrouter",
      baseUrl: "http://localhost:9/v1",
      ...options,
      fetchImpl: (async (_url: string, init?: RequestInit) => {
        seen = new Headers(init?.headers);
        return sseResponse([{ choices: [{ delta: {}, finish_reason: "stop" }] }]);
      }) as unknown as typeof fetch,
    });
    for await (const _e of client.streamChat({
      runId: "r",
      model: "m",
      messages: [{ role: "user", content: "hi" }],
    }));
    return seen;
  }

  it("sends no attribution headers by default, not even for openrouter", async () => {
    const seen = await headersFor({});
    expect(seen?.get("HTTP-Referer")).toBeNull();
    expect(seen?.get("X-Title")).toBeNull();
  });

  it("sends the caller's attribution when configured, regardless of kind", async () => {
    const seen = await headersFor({
      kind: "openai-compatible",
      appReferer: "https://example.test",
      appTitle: "Example App",
    });
    expect(seen?.get("HTTP-Referer")).toBe("https://example.test");
    expect(seen?.get("X-Title")).toBe("Example App");
  });

  it("maps appReferer/appTitle from config metadata in fromConfig", async () => {
    let seen: Headers | undefined;
    const client = OpenAiCompatibleClient.fromConfig(
      {
        id: "cfg",
        label: "Configured",
        kind: "openrouter",
        baseUrl: "http://localhost:9/v1",
        defaultModel: "m",
        enabled: true,
        metadata: { appReferer: "https://host.test", appTitle: "Host" },
      },
      (async (_url: string, init?: RequestInit) => {
        seen = new Headers(init?.headers);
        return sseResponse([{ choices: [{ delta: {}, finish_reason: "stop" }] }]);
      }) as unknown as typeof fetch,
    );
    for await (const _e of client.streamChat({
      runId: "r",
      model: "m",
      messages: [{ role: "user", content: "hi" }],
    }));
    expect(seen?.get("HTTP-Referer")).toBe("https://host.test");
    expect(seen?.get("X-Title")).toBe("Host");
  });
});

describe("OpenAiCompatibleClient capability probe", () => {
  function routedFetch(probeJson: unknown): typeof fetch {
    return (async (url: string | URL) => {
      if (String(url).endsWith("/models"))
        return jsonResponse({ data: [{ id: "m" }] });
      return jsonResponse(probeJson);
    }) as unknown as typeof fetch;
  }

  it("reports toolCalling=true when the probe returns a tool_call", async () => {
    const client = makeClient(
      routedFetch({ choices: [{ message: { tool_calls: [{ id: "c" }] } }] }),
    );
    const caps = await client.capabilities();
    expect(caps.toolCalling).toBe(true);
  });

  it("treats finish_reason=length as inconclusive (does NOT disable tools)", async () => {
    const client = makeClient(
      routedFetch({ choices: [{ message: {}, finish_reason: "length" }] }),
    );
    const caps = await client.capabilities();
    expect(caps.toolCalling).toBe(true);
  });

  it("reports toolCalling=false on a clean stop with no tool_call", async () => {
    const client = makeClient(
      routedFetch({ choices: [{ message: {}, finish_reason: "stop" }] }),
    );
    const caps = await client.capabilities();
    expect(caps.toolCalling).toBe(false);
  });
});
