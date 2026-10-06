import { describe, expect, it } from "bun:test";
import { ResponsesClient, type AiProviderContinuation } from "../src/index.js";
import {
  call,
  collect,
  encode,
  harness,
  message,
  request,
  stream,
  terminal,
  tool,
} from "./responses-helpers.js";

function failure(
  events: Awaited<ReturnType<typeof collect>>,
): string | undefined {
  const event = events.find((event) => event.type === "run.failed");
  return event?.type === "run.failed" ? event.data.errorCode : undefined;
}

function added(
  item: Record<string, unknown>,
  index = 0,
): Record<string, unknown> {
  return {
    type: "response.output_item.added",
    output_index: index,
    item: { ...item, status: "in_progress", arguments: "" },
  };
}
function args(
  id: string,
  index: number,
  delta: string,
): Record<string, unknown> {
  return {
    type: "response.function_call_arguments.delta",
    item_id: `item_${id}`,
    output_index: index,
    delta,
  };
}
function text(delta: string, index = 0): Record<string, unknown> {
  return {
    type: "response.output_text.delta",
    item_id: "message",
    output_index: index,
    content_index: 0,
    delta,
  };
}

const reasoning = {
  type: "reasoning",
  id: "reasoning",
  encrypted_content: "opaque-secret",
  summary: [],
};

describe("Responses stream conformance", () => {
  const first = call("one", "lookup", '{"value":"é"}');
  const second = call("two", "other", '{"value":null}');
  const golden = [
    added(first, 0),
    added(second, 1),
    args("one", 0, '{"value":'),
    args("two", 1, '{"value":'),
    args("one", 0, '"é"}'),
    args("two", 1, "null}"),
    {
      type: "response.function_call_arguments.done",
      item_id: "item_one",
      output_index: 0,
      arguments: first.arguments,
    },
    { type: "response.output_item.done", output_index: 0, item: first },
    { type: "response.output_item.done", output_index: 1, item: second },
    terminal([first, second]),
  ];

  it("assembles interleaved tools, exact IDs, one usage; emits only after EOF", async () => {
    const { client } = harness(golden);
    const events = await collect(client, {
      ...request,
      tools: [tool, { ...tool, name: "other" }],
    });
    expect(failure(events)).toBeUndefined();
    expect(
      events
        .filter((event) => event.type === "run.tool.requested")
        .map((event) => event.data),
    ).toEqual([
      {
        toolCallId: "one",
        toolName: "lookup",
        argumentsJson: String(first.arguments),
      },
      {
        toolCallId: "two",
        toolName: "other",
        argumentsJson: String(second.arguments),
      },
    ]);
    expect(events.filter((event) => event.type === "run.usage")).toHaveLength(
      1,
    );
    expect(events.at(-1)?.type).toBe("run.completed");
    expect(events.map((event) => event.seq)).toEqual(
      events.map((_, index) => index),
    );
  });

  it("every UTF-8 byte split and one-byte CRLF chunks match golden output", async () => {
    const { transport } = harness();
    const bytes = encode(golden, "\r\n\r\n");
    const input = { ...request, tools: [tool, { ...tool, name: "other" }] };
    for (let cut = 1; cut < bytes.length; cut++) {
      transport.request = async () => stream(bytes, [cut]);
      const events = await collect(
        new ResponsesClient({ id: "provider", transport }),
        input,
      );
      expect(failure(events)).toBeUndefined();
      expect(
        events.filter((event) => event.type === "run.tool.requested"),
      ).toHaveLength(2);
    }
    transport.request = async () =>
      stream(
        bytes,
        Array.from({ length: bytes.length - 1 }, (_, index) => index + 1),
      );
    expect(
      failure(
        await collect(
          new ResponsesClient({ id: "provider", transport }),
          input,
        ),
      ),
    ).toBeUndefined();
  });

  it.each(
    [
      [
        {
          type: "response.failed",
          response: {
            error: {
              code: "subscription_sharing_usage_limit_exceeded",
              message: "opaque-secret",
            },
          },
        },
      ],
      [{ type: "response.incomplete", response: {} }],
      [],
      ["not-json"],
      [terminal([call("unknown", "missing")])],
    ].map((ending) => ({ ending })),
  )(
    "no tool execution after partial arguments and failure",
    async ({ ending }) => {
      const { client } = harness([
        added(first),
        args("one", 0, '{"value":'),
        ...ending,
      ]);
      const events = await collect(client);
      expect(failure(events)).toBeDefined();
      expect(
        events.some(
          (event) =>
            event.type === "run.tool.requested" ||
            event.type === "run.message.completed" ||
            event.type === "run.completed",
        ),
      ).toBe(false);
      expect(JSON.stringify(events)).not.toContain("opaque-secret");
    },
  );

  it("completed followed by late failure never dispatches tools", async () => {
    const { client } = harness([
      terminal([first]),
      {
        type: "response.failed",
        response: { error: { code: "subscription_sharing_usage_unavailable" } },
      },
    ]);
    const events = await collect(client);
    expect(failure(events)).toBe("usage_limit");
    expect(events.some((event) => event.type === "run.tool.requested")).toBe(
      false,
    );
  });

  it("final output reconciles text and tool suffix without duplicates", async () => {
    const { client } = harness([
      added(message("Hi")),
      text("H"),
      terminal([message("Hi"), first]),
    ]);
    const events = await collect(client);
    expect(failure(events)).toBeUndefined();
    expect(
      events
        .filter((event) => event.type === "run.message.delta")
        .map((event) => event.data.delta)
        .join(""),
    ).toBe("Hi");
    expect(
      events.filter((event) => event.type === "run.tool.requested"),
    ).toHaveLength(1);
  });

  it.each(
    [
      [terminal([first, { ...first, id: "other" }])],
      [added(first), added({ ...first, id: "another" })],
      [added(first), args("one", 1, "{}")],
      [added(first), args("one", 0, "bad"), terminal([first])],
      [added(first), terminal([])],
      [added(message("a")), text("a"), terminal([message("b")])],
      [terminal([call("invalid", "lookup", "{")])],
    ].map((events) => ({ events })),
  )(
    "rejects malformed identities/arguments/final reconciliation",
    async ({ events }) => {
      const result = await collect(harness(events).client);
      expect(failure(result)).toBeDefined();
      expect(result.some((event) => event.type === "run.tool.requested")).toBe(
        false,
      );
    },
  );

  it("empty completed and reasoning-only completed are distinct valid results", async () => {
    expect(
      failure(await collect(harness([terminal([])]).client)),
    ).toBeUndefined();
    let state: AiProviderContinuation | undefined;
    let activity = 0;
    const { client } = harness([
      added(reasoning),
      {
        type: "response.reasoning_summary_text.delta",
        summary_index: 0,
        item_id: "reasoning",
        output_index: 0,
        delta: "private thought",
      },
      terminal([reasoning]),
    ]);
    const events = await collect(client, {
      ...request,
      continuationScope: { chatId: "chat", branchId: "branch" },
      onContinuation(value) {
        state = value;
      },
      onActivity() {
        activity++;
      },
    });
    expect(failure(events)).toBeUndefined();
    expect(state?.inputItems.at(-1)).toEqual(reasoning);
    expect(JSON.stringify(events)).not.toContain("private thought");
    expect(JSON.stringify(events)).not.toContain("opaque-secret");
    expect(activity).toBe(3);
  });

  it.each([undefined, {}])(
    "missing opaque reasoning cannot pass as success",
    async (extra) => {
      const { client } = harness([
        terminal([{ ...reasoning, encrypted_content: undefined }]),
      ]);
      expect(failure(await collect(client, { ...request, ...extra }))).toBe(
        "continuation_required",
      );
    },
  );

  it("oversize frames and data-line frames fail bounded", async () => {
    for (const bytes of [
      new TextEncoder().encode(`data: ${"x".repeat(1024 * 1024 + 1)}`),
      new TextEncoder().encode(
        `${`data: ${"x".repeat(1024)}\n`.repeat(1100)}\n`,
      ),
    ]) {
      const { transport } = harness();
      transport.request = async () => stream(bytes);
      expect(
        failure(
          await collect(new ResponsesClient({ id: "provider", transport })),
        ),
      ).toBeDefined();
    }
  });
});
