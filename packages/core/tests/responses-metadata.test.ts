import { describe, expect, it } from "bun:test";
import type { AiProviderContinuation } from "../src/index.js";
import {
  call,
  collect,
  harness,
  message,
  request,
  terminal,
} from "./responses-helpers.js";

const metadataSecret = "metadata-private-canary";
const metadata = {
  type: "response.metadata",
  metadata: { fixture: metadataSecret },
};

/** Synthetic payload; only the event name/order was observed in the live SIWC trace. */
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
      item: { ...message(""), status: "in_progress", content: [] },
    },
    {
      type: "response.content_part.added",
      item_id: "message",
      output_index: 0,
      content_index: 0,
      part: { type: "output_text", text: "", annotations: [] },
    },
    {
      type: "response.output_text.delta",
      item_id: "message",
      output_index: 0,
      content_index: 0,
      delta: "Hello",
    },
    {
      type: "response.output_text.delta",
      item_id: "message",
      output_index: 0,
      content_index: 0,
      delta: " there",
    },
    metadata,
    {
      type: "response.output_text.done",
      item_id: "message",
      output_index: 0,
      content_index: 0,
      text: "Hello there",
    },
    {
      type: "response.content_part.done",
      item_id: "message",
      output_index: 0,
      content_index: 0,
      part: { type: "output_text", text: "Hello there", annotations: [] },
    },
    {
      type: "response.output_item.done",
      output_index: 0,
      item: message("Hello there"),
    },
    terminal([message("Hello there")]),
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

describe("Responses live-observed inert metadata extension", () => {
  it("replays streaming text, metadata and completed response without replacing text with failure", async () => {
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
    ).toEqual(["Hello", " there"]);
    expect(
      events.find((event) => event.type === "run.message.completed")?.data
        .content,
    ).toBe("Hello there");
    expect(events.at(-1)?.type).toBe("run.completed");
    expect(events.filter((event) => event.type === "run.usage")).toHaveLength(
      1,
    );
    expect(continuation?.inputItems.at(-1)).toEqual(message("Hello there"));
    expect(JSON.stringify({ events, continuation })).not.toContain(
      metadataSecret,
    );
  });

  it("metadata cannot inject text, tools, usage, continuation or terminal state", async () => {
    const events = await collect(
      harness([
        {
          ...metadata,
          delta: "injected-text",
          output: [call("injected")],
          response: {
            status: "completed",
            output: [call("injected")],
            usage: { input_tokens: 999 },
          },
          error: { code: "subscription_sharing_usage_limit_exceeded" },
        },
        terminal([]),
      ]).client,
    );
    expect(failure(events)).toBeUndefined();
    expect(
      events.some(
        (event) =>
          event.type === "run.tool.requested" ||
          event.type === "run.message.delta",
      ),
    ).toBe(false);
    expect(events.filter((event) => event.type === "run.usage")).toHaveLength(
      1,
    );
    expect(
      events.find((event) => event.type === "run.usage")?.data.promptTokens,
    ).toBe(2);
    expect(JSON.stringify(events)).not.toContain("injected");
    expect(JSON.stringify(events)).not.toContain(metadataSecret);
  });

  it("metadata without a completed response fails EOF and never persists continuation", async () => {
    let persisted = false;
    const events = await collect(harness([metadata]).client, {
      ...request,
      continuationScope: { chatId: "chat", branchId: "branch" },
      onContinuation() {
        persisted = true;
      },
    });
    expect(failure(events)).toBe("stream_incomplete");
    expect(successful(events)).toBe(false);
    expect(persisted).toBe(false);
  });

  it.each([
    {
      event: { type: "response.metadata.delta" },
      code: "unsupported_stream_event",
    },
    { event: { type: "new.event" }, code: "unsupported_stream_event" },
    {
      event: {
        type: "response.failed",
        response: { error: { code: "subscription_sharing_usage_unavailable" } },
      },
      code: "service_unavailable",
    },
    {
      event: { type: "response.incomplete", response: {} },
      code: "response_incomplete",
    },
  ])("metadata never suppresses $code", async ({ event, code }) => {
    const events = await collect(
      harness([metadata, event, terminal([call("one")])]).client,
    );
    expect(failure(events)).toBe(code);
    expect(successful(events)).toBe(false);
  });

  it("metadata after terminal still fails instead of dispatching pending tools", async () => {
    const events = await collect(
      harness([terminal([call("one")]), metadata]).client,
    );
    expect(failure(events)).toBe("event_after_terminal");
    expect(successful(events)).toBe(false);
  });

  it("metadata does not bypass final text reconciliation", async () => {
    const frames = textFrames();
    frames[frames.length - 1] = terminal([message("Wrong")]);
    const events = await collect(harness(frames).client);
    expect(failure(events)).toBe("stream_output_mismatch");
    expect(successful(events)).toBe(false);
  });

  it("metadata remains subject to frame and cumulative stream byte limits", async () => {
    for (const events of [
      [
        { type: "response.metadata", metadata: "x".repeat(1024 * 1024 + 1) },
        terminal(),
      ],
      [
        ...Array.from({ length: 18 }, () => ({
          type: "response.metadata",
          metadata: "x".repeat(1000 * 1000),
        })),
        terminal(),
      ],
    ]) {
      const result = await collect(harness(events).client);
      expect(failure(result)).toBeDefined();
      expect(successful(result)).toBe(false);
    }
  });
});
