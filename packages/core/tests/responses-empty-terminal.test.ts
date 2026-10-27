import { describe, expect, it } from "bun:test";
import { ResponsesClient, type AiProviderContinuation } from "../src/index.js";
import {
  call,
  collect,
  encode,
  harness,
  message,
  request,
  terminal,
} from "./responses-helpers.js";

/** Synthetic content reproduces live SIWC item identities, done status, and empty terminal output. */
function textFrames(): Record<string, unknown>[] {
  return [
    {
      type: "response.created",
      response: { id: "response", status: "in_progress" },
    },
    {
      type: "response.in_progress",
      response: { id: "response", status: "in_progress" },
    },
    {
      type: "response.output_item.added",
      output_index: 0,
      item: { ...message(""), content: [], status: "in_progress" },
    },
    {
      type: "response.content_part.added",
      output_index: 0,
      item_id: "message",
      content_index: 0,
      part: { type: "output_text", text: "" },
    },
    {
      type: "response.output_text.delta",
      output_index: 0,
      item_id: "message",
      content_index: 0,
      delta: "Hello",
    },
    { type: "response.metadata", metadata: { canary: "private-metadata" } },
    {
      type: "response.output_text.done",
      output_index: 0,
      item_id: "message",
      content_index: 0,
      text: "Hello",
    },
    {
      type: "response.content_part.done",
      output_index: 0,
      item_id: "message",
      content_index: 0,
      part: { type: "output_text", text: "Hello" },
    },
    {
      type: "response.output_item.done",
      output_index: 0,
      item: message("Hello"),
    },
    terminal([]),
  ];
}
function failure(
  events: Awaited<ReturnType<typeof collect>>,
): string | undefined {
  const event = events.find((event) => event.type === "run.failed");
  return event?.type === "run.failed" ? event.data.errorCode : undefined;
}
function successful(events: Awaited<ReturnType<typeof collect>>): boolean {
  return events.some(
    (event) =>
      event.type === "run.completed" ||
      event.type === "run.message.completed" ||
      event.type === "run.tool.requested",
  );
}
function itemFrames(
  item: Record<string, unknown>,
  index = 0,
): Record<string, unknown>[] {
  return [
    {
      type: "response.output_item.added",
      output_index: index,
      item: { ...item, status: "in_progress" },
    },
    { type: "response.output_item.done", output_index: index, item },
  ];
}

describe("Responses ChatGPT empty terminal output compatibility", () => {
  it("replays completed message omitted from empty account terminal output", async () => {
    let continuation: AiProviderContinuation | undefined;
    const events = await collect(
      harness(textFrames(), "chatgpt-account").client,
      {
        ...request,
        continuationScope: { chatId: "chat", branchId: "branch" },
        onContinuation(value) {
          continuation = value;
        },
      },
    );
    expect(failure(events)).toBeUndefined();
    expect(
      events
        .filter((event) => event.type === "run.message.delta")
        .map((event) => event.data.delta),
    ).toEqual(["Hello"]);
    expect(
      events.find((event) => event.type === "run.message.completed")?.data
        .content,
    ).toBe("Hello");
    expect(events.at(-1)?.type).toBe("run.completed");
    expect(continuation?.inputItems.at(-1)).toEqual(message("Hello"));
    expect(JSON.stringify({ events, continuation })).not.toContain(
      "private-metadata",
    );
  });

  it("API-key profile retains missing-output rejection for same stream", async () => {
    const events = await collect(harness(textFrames()).client);
    expect(failure(events)).toBe("missing_output_item");
    expect(successful(events)).toBe(false);
  });

  it.each(
    [
      [
        {
          type: "response.output_item.added",
          output_index: 0,
          item: { ...message(""), status: "in_progress" },
        },
        terminal([]),
      ],
      [...itemFrames(message("Hello"), 1), terminal([])],
      [
        ...itemFrames(message("Hello")),
        ...itemFrames(message("Extra", "extra"), 1),
        terminal([message("Hello")]),
      ],
    ].map((frames) => ({ frames })),
  )(
    "never reconstructs partial, noncontiguous or nonempty missing output",
    async ({ frames }) => {
      const events = await collect(harness(frames, "chatgpt-account").client);
      expect(failure(events)).toBe("missing_output_item");
      expect(successful(events)).toBe(false);
    },
  );

  it("nonempty conflicting terminal output remains an error", async () => {
    const frames = textFrames();
    frames[frames.length - 1] = terminal([message("Wrong")]);
    const events = await collect(harness(frames, "chatgpt-account").client);
    expect(failure(events)).toBe("stream_output_mismatch");
    expect(successful(events)).toBe(false);
  });

  it.each(["in_progress", "incomplete", undefined])(
    "does not promote non-completed message status %s",
    async (status) => {
      const events = await collect(
        harness(
          [...itemFrames({ ...message("Hello"), status }), terminal([])],
          "chatgpt-account",
        ).client,
      );
      expect(failure(events)).toBeDefined();
      expect(successful(events)).toBe(false);
    },
  );

  it("uses finished function call only after completed terminal and EOF", async () => {
    const toolCall = call("one");
    const frames = [...itemFrames(toolCall), terminal([])];
    const { transport } = harness([], "chatgpt-account");
    let finish!: () => void;
    let opened!: () => void;
    const ready = new Promise<void>((resolve) => {
      opened = resolve;
    });
    transport.request = async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(encode(frames));
            finish = () => controller.close();
            opened();
          },
        }),
      );
    const events: Awaited<ReturnType<typeof collect>> = [];
    const collecting = (async () => {
      for await (const event of new ResponsesClient({
        id: "provider",
        transport,
      }).streamChat(request))
        events.push(event);
    })();
    await ready;
    expect(events.some((event) => event.type === "run.tool.requested")).toBe(
      false,
    );
    finish();
    await collecting;
    expect(failure(events)).toBeUndefined();
    expect(
      events
        .filter((event) => event.type === "run.tool.requested")
        .map((event) => event.data.toolCallId),
    ).toEqual(["one"]);
  });

  it("preserves final encrypted reasoning privately from done item", async () => {
    const reasoning = {
      type: "reasoning",
      id: "reason",
      encrypted_content: "opaque-private",
      summary: [{ type: "summary_text", text: "Public summary" }],
    };
    let continuation: AiProviderContinuation | undefined;
    const events = await collect(
      harness([...itemFrames(reasoning), terminal([])], "chatgpt-account")
        .client,
      {
        ...request,
        continuationScope: { chatId: "chat", branchId: "branch" },
        onContinuation(value) {
          continuation = value;
        },
      },
    );
    expect(failure(events)).toBeUndefined();
    expect(continuation?.inputItems.at(-1)).toEqual(reasoning);
    expect(JSON.stringify(events)).not.toContain("opaque-private");
  });

  it.each(
    [
      [],
      [
        {
          type: "response.failed",
          response: {
            error: { code: "subscription_sharing_usage_unavailable" },
          },
        },
      ],
      [{ type: "response.incomplete", response: {} }],
      [
        terminal([]),
        {
          type: "response.failed",
          response: {
            error: { code: "subscription_sharing_usage_unavailable" },
          },
        },
      ],
      [terminal([]), { type: "response.metadata" }],
    ].map((ending) => ({ ending })),
  )(
    "finished items never substitute for valid terminal and clean EOF",
    async ({ ending }) => {
      const events = await collect(
        harness([...itemFrames(call("one")), ...ending], "chatgpt-account")
          .client,
      );
      expect(failure(events)).toBeDefined();
      expect(successful(events)).toBe(false);
    },
  );

  it("missing encrypted reasoning and malformed tool arguments still fail", async () => {
    for (const item of [
      { type: "reasoning", id: "reason", summary: [] },
      call("bad", "lookup", "{"),
    ]) {
      const events = await collect(
        harness([...itemFrames(item), terminal([])], "chatgpt-account").client,
      );
      expect(failure(events)).toBeDefined();
      expect(successful(events)).toBe(false);
    }
  });
});
