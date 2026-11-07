import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import type { AiRunEvent } from "@agentkit/contracts";

// ONEMIND-352: provider citation annotations must survive the provider
// layer as contract `citations` on run.message.delta / run.message.completed.
// Mirror of the Rust `chat_stream_surfaces_url_citation_annotations…` test.

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

async function collect(chunks: unknown[]): Promise<AiRunEvent[]> {
  const client = new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost:9/v1",
    fetchImpl: (async () =>
      sseResponse(chunks)) as unknown as typeof fetch,
  });
  const out: AiRunEvent[] = [];
  for await (const e of client.streamChat({
    runId: "r",
    model: "m",
    messages: [{ role: "user", content: "hi" }],
  }))
    out.push(e);
  return out;
}

describe("openai-compatible citations", () => {
  it("surfaces url_citation annotations and plain citations on delta and completed", async () => {
    const events = await collect([
      {
        choices: [
          {
            index: 0,
            delta: {
              role: "assistant",
              content: "",
              annotations: [
                {
                  index: 0,
                  type: "url_citation",
                  url_citation: {
                    title: "Example Capital Report",
                    url: "https://example.com/capital-report",
                    start_index: 47,
                    end_index: 68,
                  },
                },
              ],
            },
            finish_reason: null,
          },
        ],
      },
      { choices: [{ index: 0, delta: { content: "Paris." } }] },
      {
        choices: [
          {
            index: 0,
            delta: {
              citations: [
                { url: "https://demo.example/census", title: "Demo Census" },
                "https://plain.example/only",
                { url: "https://example.com/capital-report" },
              ],
            },
          },
        ],
      },
      { choices: [{ index: 0, delta: {}, finish_reason: "stop" }] },
    ]);

    expect(
      events
        .filter((e) => e.type === "run.message.delta" && e.data.citations)
        .map((e) => (e.type === "run.message.delta" ? e.data.citations : [])),
    ).toEqual([
      [
        {
          url: "https://example.com/capital-report",
          title: "Example Capital Report",
        },
      ],
      [
        { url: "https://demo.example/census", title: "Demo Census" },
        { url: "https://plain.example/only" },
      ],
    ]);
    // Text still flows as an ordinary content delta.
    expect(
      events.some(
        (e) =>
          e.type === "run.message.delta" && e.data.delta === "Paris.",
      ),
    ).toBe(true);
    const completed = events.find((e) => e.type === "run.message.completed");
    expect(completed?.type).toBe("run.message.completed");
    if (completed?.type === "run.message.completed")
      expect(completed.data.citations).toEqual([
        {
          url: "https://example.com/capital-report",
          title: "Example Capital Report",
        },
        { url: "https://demo.example/census", title: "Demo Census" },
        { url: "https://plain.example/only" },
      ]);
  });

  it("never adds a citations field on the ordinary lane", async () => {
    const events = await collect([
      { choices: [{ delta: { content: "plain" }, finish_reason: "stop" }] },
    ]);
    for (const e of events) {
      if (e.type === "run.message.delta" || e.type === "run.message.completed")
        expect(e.data.citations).toBeUndefined();
    }
    expect(
      events.some(
        (e) => e.type === "run.message.delta" && e.data.delta === "plain",
      ),
    ).toBe(true);
  });
});
