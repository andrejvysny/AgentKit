import { expect, test } from "bun:test";
import {
  collect,
  harness,
  message,
  request,
  terminal,
} from "./responses-helpers.js";

const reasoning = {
  type: "reasoning",
  id: "reason",
  encrypted_content: "opaque-secret",
  summary: [],
};
const base = { item_id: "reason", output_index: 0, summary_index: 0 };

test("Responses public reasoning deltas, done and final values do not duplicate", async () => {
  const events = await collect(
    harness([
      { type: "response.output_item.added", output_index: 0, item: reasoning },
      {
        ...base,
        type: "response.reasoning_summary_part.added",
        part: { type: "summary_text", text: "" },
      },
      {
        ...base,
        type: "response.reasoning_summary_text.delta",
        delta: "Think Ž🙂",
      },
      {
        ...base,
        type: "response.reasoning_summary_text.delta",
        delta: " harder",
      },
      {
        ...base,
        type: "response.reasoning_summary_text.done",
        text: "Think Ž🙂 harder",
      },
      {
        ...base,
        type: "response.reasoning_summary_part.done",
        part: { type: "summary_text", text: "Think Ž🙂 harder" },
      },
      terminal([
        {
          ...reasoning,
          summary: [{ type: "summary_text", text: "Think Ž🙂 harder" }],
        },
        message("Answer"),
      ]),
    ]).client,
    {
      ...request,
      continuationScope: { chatId: "chat", branchId: "branch" },
      onContinuation() {},
    },
  );
  expect(
    events
      .filter((event) => event.type === "run.message.reasoning-delta")
      .map((event) => event.data.delta),
  ).toEqual(["Think Ž🙂", " harder"]);
  const completed = events.find(
    (event) => event.type === "run.message.completed",
  );
  expect(completed?.data.reasoningContent).toBe("Think Ž🙂 harder");
  expect(completed?.data.content).toBe("Answer");
  expect(JSON.stringify(events)).not.toContain("opaque-secret");
});

test("Responses final-only reasoning is retained and conflicting done text fails", async () => {
  const input = {
    ...request,
    continuationScope: { chatId: "chat", branchId: "branch" },
    onContinuation() {},
  };
  const final = await collect(
    harness([
      terminal([
        {
          ...reasoning,
          summary: [{ type: "summary_text", text: "Final summary" }],
        },
      ]),
    ]).client,
    input,
  );
  expect(
    final.find((event) => event.type === "run.message.completed")?.data
      .reasoningContent,
  ).toBe("Final summary");
  const failed = await collect(
    harness([
      { type: "response.output_item.added", output_index: 0, item: reasoning },
      {
        ...base,
        type: "response.reasoning_summary_text.delta",
        delta: "First",
      },
      { ...base, type: "response.reasoning_summary_text.done", text: "Wrong" },
      terminal([reasoning]),
    ]).client,
    input,
  );
  expect(
    failed.find((event) => event.type === "run.failed")?.data.errorCode,
  ).toBe("stream_output_mismatch");
});
