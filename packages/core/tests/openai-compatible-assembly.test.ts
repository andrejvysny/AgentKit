import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import type { AiRunEvent, AiToolCall } from "@agentkit/contracts";

const enc = new TextEncoder();

/** A well-formed stream: one frame per chunk, terminated by `[DONE]`. */
function sseResponse(chunks: unknown[]): Response {
  return sseBody((c) => {
    for (const chunk of chunks)
      c.enqueue(enc.encode(`data: ${JSON.stringify(chunk)}\n\n`));
    c.enqueue(enc.encode("data: [DONE]\n\n"));
    c.close();
  });
}

function sseBody(
  start: (controller: ReadableStreamDefaultController<Uint8Array>) => void,
): Response {
  return new Response(new ReadableStream<Uint8Array>({ start }), {
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

const warningCodes = (events: AiRunEvent[]): string[] =>
  events
    .filter((e) => e.type === "run.warning")
    .map((e) => (e as { data: { code: string } }).data.code);

describe("OpenAiCompatibleClient tool-call assembly", () => {
  async function assemble(
    chunks: unknown[],
  ): Promise<{ events: AiRunEvent[]; calls: AiToolCall[] }> {
    const client = makeClient((async () =>
      sseResponse(chunks)) as unknown as typeof fetch);
    const events = await collectStream(client);
    const completed = events.find(
      (e) => e.type === "run.message.completed",
    ) as AiRunEvent & { data: { toolCalls?: AiToolCall[] } };
    return { events, calls: completed.data.toolCalls ?? [] };
  }

  it("keeps two calls apart when the provider sends no index", async () => {
    // The llama.cpp/vLLM/gateway shape: the id arrives on the opening delta and
    // its arguments trail it with no index at all. Keying on index alone
    // collapsed both calls into one with concatenated arguments.
    const { calls } = await assemble([
      {
        choices: [
          { delta: { tool_calls: [{ id: "a", function: { name: "one" } }] } },
        ],
      },
      {
        choices: [
          { delta: { tool_calls: [{ function: { arguments: '{"x":1}' } }] } },
        ],
      },
      {
        choices: [
          { delta: { tool_calls: [{ id: "b", function: { name: "two" } }] } },
        ],
      },
      {
        choices: [
          { delta: { tool_calls: [{ function: { arguments: '{"y":2}' } }] } },
        ],
      },
      { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
    ]);
    expect(calls).toEqual([
      { id: "a", name: "one", argumentsJson: '{"x":1}' },
      { id: "b", name: "two", argumentsJson: '{"y":2}' },
    ]);
  });

  it("still assembles the ordinary indexed shape", async () => {
    const { calls } = await assemble([
      {
        choices: [
          {
            delta: {
              tool_calls: [
                { index: 0, id: "c1", function: { name: "echo" } },
                { index: 1, id: "c2", function: { name: "echo" } },
              ],
            },
          },
        ],
      },
      {
        choices: [
          {
            delta: {
              tool_calls: [
                { index: 0, function: { arguments: '{"t":"a"}' } },
                { index: 1, function: { arguments: '{"t":"b"}' } },
              ],
            },
          },
        ],
      },
      { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
    ]);
    expect(calls).toEqual([
      { id: "c1", name: "echo", argumentsJson: '{"t":"a"}' },
      { id: "c2", name: "echo", argumentsJson: '{"t":"b"}' },
    ]);
  });

  it("re-keys a reused tool call id and warns", async () => {
    const { events, calls } = await assemble([
      {
        choices: [
          {
            delta: {
              tool_calls: [
                {
                  index: 0,
                  id: "dup",
                  function: { name: "one", arguments: "{}" },
                },
              ],
            },
          },
        ],
      },
      {
        choices: [
          {
            delta: {
              tool_calls: [
                {
                  index: 1,
                  id: "dup",
                  function: { name: "two", arguments: "{}" },
                },
              ],
            },
          },
        ],
      },
      { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
    ]);
    // Both calls stay separately answerable: one tool_call_id each.
    expect(calls.map((c) => c.id)).toEqual(["dup", "dup#2"]);
    expect(warningCodes(events)).toContain("duplicate_tool_call_id");
  });
});

describe("OpenAiCompatibleClient stream completeness", () => {
  it("says incomplete when the stream ends with neither [DONE] nor a finish_reason", async () => {
    const client = makeClient((async () =>
      sseBody((c) => {
        c.enqueue(
          enc.encode(
            `data: ${JSON.stringify({
              choices: [{ delta: { content: "half an ans" } }],
            })}\n\n`,
          ),
        );
        c.close();
      })) as unknown as typeof fetch);
    const events = await collectStream(client);
    const completed = events.find(
      (e) => e.type === "run.message.completed",
    ) as AiRunEvent & { data: { finishReason?: string } };
    // "stop" here would claim a cut-off answer had finished.
    expect(completed.data.finishReason).toBe("incomplete");
    expect(warningCodes(events)).toContain("stream_incomplete");
  });

  it("does not say incomplete when the provider sent [DONE]", async () => {
    const client = makeClient((async () =>
      sseResponse([
        { choices: [{ delta: { content: "hi" } }] },
      ])) as unknown as typeof fetch);
    const events = await collectStream(client);
    expect(warningCodes(events)).not.toContain("stream_incomplete");
  });

  it("emits usage counted before a mid-stream failure, marked not final", async () => {
    // The usage frame must be READ before the socket dies, so it is delivered
    // on the first pull and the error only on the second.
    let delivered = false;
    const client = makeClient(
      (async () =>
        new Response(
          new ReadableStream<Uint8Array>({
            pull(c) {
              if (delivered) {
                c.error(new Error("socket reset"));
                return;
              }
              delivered = true;
              c.enqueue(
                enc.encode(
                  `data: ${JSON.stringify({
                    usage: { prompt_tokens: 11, completion_tokens: 2 },
                  })}\n\n`,
                ),
              );
            },
          }),
          { status: 200, headers: { "content-type": "text/event-stream" } },
        )) as unknown as typeof fetch,
    );
    const events = await collectStream(client);
    const usage = events.find((e) => e.type === "run.usage") as AiRunEvent & {
      data: { promptTokens?: number; finalForCall: boolean };
    };
    // The tokens were spent whether or not the call finished.
    expect(usage.data.promptTokens).toBe(11);
    expect(usage.data.finalForCall).toBe(false);
    expect(events.at(-1)!.type).toBe("run.failed");
  });
});

describe("OpenAiCompatibleClient header precedence", () => {
  it("cannot be spoofed by a differently-cased extraHeaders entry", async () => {
    let seen: Headers | undefined;
    const client = new OpenAiCompatibleClient({
      id: "gateway",
      kind: "host-gateway",
      baseUrl: "http://localhost:9/v1",
      apiKey: "real-bearer",
      // Capitalised: a plain object merge kept BOTH keys and fetch joined them
      // into "Bearer SPOOFED, Bearer real-bearer".
      extraHeaders: { Authorization: "Bearer SPOOFED", Accept: "text/spoofed" },
      fetchImpl: (async (_url: string, init?: RequestInit) => {
        seen = new Headers(init?.headers);
        return sseResponse([
          { choices: [{ delta: {}, finish_reason: "stop" }] },
        ]);
      }) as unknown as typeof fetch,
    });
    await collectStream(client);
    expect(seen?.get("authorization")).toBe("Bearer real-bearer");
    expect(seen?.get("accept")).toBe("application/json");
  });
});
