import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { MockProviderClient } from "@agentkit/testing";
import { collectRun } from "./helpers.js";
import type { AiChatRequest } from "../src/providers/client.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";

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

/** Stream one turn and hand back both the wire body and the events. */
async function send(messages: AiChatMessage[]): Promise<{
  wire: Record<string, unknown>;
  events: AiRunEvent[];
}> {
  let wire: Record<string, unknown> = {};
  const client = new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost:9/v1",
    fetchImpl: (async (_url: string | URL, init?: RequestInit) => {
      wire = JSON.parse(String(init?.body)) as Record<string, unknown>;
      return sseResponse([{ choices: [{ delta: {}, finish_reason: "stop" }] }]);
    }) as unknown as typeof fetch,
  });
  const events: AiRunEvent[] = [];
  for await (const e of client.streamChat({ runId: "r", model: "m", messages }))
    events.push(e);
  return { wire, events };
}

const flattenedWarnings = (events: AiRunEvent[]) =>
  events.filter(
    (e) =>
      e.type === "run.warning" &&
      (e as { data: { code: string } }).data.code === "multimodal_flattened",
  );

describe("OpenAiCompatibleClient multimodal request mapping", () => {
  it("maps a user parts array onto OpenAI content parts, detail included", async () => {
    const { wire, events } = await send([
      {
        role: "user",
        content: [
          { type: "text", text: "what is this?" },
          {
            type: "image",
            source: { kind: "url", url: "https://example.test/board.png" },
            detail: "high",
          },
        ],
      },
    ]);
    expect(wire.messages).toEqual([
      {
        role: "user",
        content: [
          { type: "text", text: "what is this?" },
          {
            type: "image_url",
            image_url: {
              url: "https://example.test/board.png",
              detail: "high",
            },
          },
        ],
      },
    ]);
    // Nothing was lost, so nothing is warned about.
    expect(flattenedWarnings(events).length).toBe(0);
  });

  it("inlines a data image as a data: URL and omits an absent detail", async () => {
    const { wire } = await send([
      {
        role: "assistant",
        content: [
          {
            type: "image",
            source: {
              kind: "data",
              base64: "aGVsbG8=",
              mediaType: "image/png",
            },
          },
        ],
      },
    ]);
    expect(wire.messages).toEqual([
      {
        role: "assistant",
        content: [
          {
            type: "image_url",
            image_url: { url: "data:image/png;base64,aGVsbG8=" },
          },
        ],
      },
    ]);
  });

  it("flattens a system message's parts to text and warns exactly once", async () => {
    const { wire, events } = await send([
      {
        role: "system",
        content: [
          { type: "text", text: "be terse" },
          {
            type: "image",
            source: { kind: "url", url: "https://example.test/logo.png" },
          },
          { type: "text", text: "cite sources" },
        ],
      },
      { role: "user", content: "hi" },
    ]);
    expect(wire.messages).toEqual([
      { role: "system", content: "be terse\ncite sources" },
      { role: "user", content: "hi" },
    ]);
    const warnings = flattenedWarnings(events);
    expect(warnings.length).toBe(1);
    expect(
      (warnings[0] as { data: { message: string } }).data.message,
    ).toContain("system");
    // The request still went out — a dropped image degrades a turn, it does not
    // fail it.
    expect(events.some((e) => e.type === "run.failed")).toBe(false);
    expect(events.some((e) => e.type === "run.message.completed")).toBe(true);
  });

  it("flattens a tool message's parts and names both roles in one warning", async () => {
    const { wire, events } = await send([
      {
        role: "system",
        content: [
          { type: "text", text: "sys" },
          {
            type: "image",
            source: { kind: "url", url: "https://example.test/a.png" },
          },
        ],
      },
      { role: "user", content: "hi" },
      {
        role: "tool",
        toolCallId: "c1",
        content: [
          { type: "text", text: '{"ok":true}' },
          {
            type: "image",
            source: {
              kind: "data",
              base64: "aGVsbG8=",
              mediaType: "image/png",
            },
          },
        ],
      },
    ]);
    expect(wire.messages).toEqual([
      { role: "system", content: "sys" },
      { role: "user", content: "hi" },
      { role: "tool", content: '{"ok":true}', tool_call_id: "c1" },
    ]);
    const warnings = flattenedWarnings(events);
    expect(warnings.length).toBe(1);
    const message = (warnings[0] as { data: { message: string } }).data.message;
    expect(message).toContain("system");
    expect(message).toContain("tool");
  });

  it("does not warn when a flattened role carried text parts only", async () => {
    const { wire, events } = await send([
      { role: "system", content: [{ type: "text", text: "be terse" }] },
      { role: "user", content: "hi" },
    ]);
    expect(wire.messages).toEqual([
      { role: "system", content: "be terse" },
      { role: "user", content: "hi" },
    ]);
    // Flattening a text-only parts array loses nothing.
    expect(flattenedWarnings(events).length).toBe(0);
  });

  it("leaves plain-string messages exactly as they were before multimodal", async () => {
    const { wire, events } = await send([
      { role: "system", content: "be terse" },
      { role: "user", content: "hi", name: "ada" },
      {
        role: "assistant",
        content: "calling",
        toolCalls: [{ id: "c1", name: "echo", argumentsJson: '{"text":"x"}' }],
      },
      { role: "tool", content: '{"ok":true}', toolCallId: "c1" },
    ]);
    expect(wire.messages).toEqual([
      { role: "system", content: "be terse" },
      { role: "user", content: "hi", name: "ada" },
      {
        role: "assistant",
        content: "calling",
        tool_calls: [
          {
            id: "c1",
            type: "function",
            function: { name: "echo", arguments: '{"text":"x"}' },
          },
        ],
      },
      { role: "tool", content: '{"ok":true}', tool_call_id: "c1" },
    ]);
    expect(flattenedWarnings(events).length).toBe(0);
  });
});

/** Records the messages each provider call received, then delegates. */
class RecordingMockClient extends MockProviderClient {
  readonly calls: AiChatMessage[][] = [];

  override async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.calls.push(input.messages);
    yield* super.streamChat(input);
  }
}

/** An echo tool, so a scripted tool call has something real to execute. */
const echoTool: AiTool<{ text?: string }, { echoed: string }> = {
  definition: {
    name: "echo",
    version: "1.0.0",
    effect: "read",
    capability: "echo",
    description: "Echo the input.",
    inputSchema: { type: "object", properties: { text: { type: "string" } } },
  },
  async execute(ctx, input) {
    return {
      ok: true,
      data: { echoed: input.text ?? "" },
      summary: "echoed",
      sources: [],
      warnings: [],
      truncated: false,
      limits: ctx.limits,
    };
  },
};

describe("runChat passthrough of multimodal content", () => {
  it("hands the provider the parts array untouched and completes normally", async () => {
    const client = new RecordingMockClient();
    client.setScript([{ steps: [{ kind: "text", content: "a board." }] }]);
    const content: AiChatMessage["content"] = [
      { type: "text", text: "what is this?" },
      {
        type: "image",
        source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
        detail: "low",
      },
    ];
    const { events, result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content }],
        limits: resolveToolLimits({ preference: "small" }),
        runId: "run-multimodal",
        firstSeq: 0,
      }),
    );
    expect(result.terminal).toBe("completed");
    expect(events.at(-1)?.type).toBe("run.completed");
    // The loop copies the message list but must not touch the body.
    expect(client.calls[0]?.[0]?.content).toEqual(content);
  });

  it("warns about a flattened system message once per RUN, not once per call", async () => {
    // Two provider round-trips over the SAME flattened system message. The
    // client has no idea it is being called twice and re-derives the warning
    // every time; only the loop knows, so only the loop can collapse it.
    let call = 0;
    const client = new OpenAiCompatibleClient({
      id: "test",
      kind: "openai-compatible",
      baseUrl: "http://localhost:9/v1",
      fetchImpl: (async () => {
        call += 1;
        return call === 1
          ? sseResponse([
              {
                choices: [
                  {
                    delta: {
                      tool_calls: [
                        {
                          index: 0,
                          id: "c1",
                          function: { name: "echo", arguments: '{"text":"x"}' },
                        },
                      ],
                    },
                  },
                ],
              },
              { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
            ])
          : sseResponse([
              { choices: [{ delta: { content: "done" } }] },
              { choices: [{ delta: {}, finish_reason: "stop" }] },
            ]);
      }) as unknown as typeof fetch,
    });
    const registry = new AiToolRegistry();
    registry.register(echoTool as AiTool);

    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [
          {
            role: "system",
            content: [
              { type: "text", text: "be terse" },
              {
                type: "image",
                source: { kind: "url", url: "https://example.test/a.png" },
              },
            ],
          },
          { role: "user", content: "hi" },
        ],
        limits: resolveToolLimits({ preference: "small" }),
        runId: "run-flatten-once",
        firstSeq: 0,
      }),
    );

    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(2);
    expect(call).toBe(2);
    expect(flattenedWarnings(events).length).toBe(1);
  });
});
