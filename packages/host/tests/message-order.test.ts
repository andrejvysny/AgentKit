import { describe, expect, it } from "bun:test";
import { orderMessagesForProvider } from "../src/turn/message-order.js";
import type { MessageRecord } from "../src/index.js";

let seq = 0;

function record(
  overrides: Partial<MessageRecord> & Pick<MessageRecord, "role">,
): MessageRecord {
  seq += 1;
  return {
    id: `m${seq}`,
    chatId: "chat-1",
    content: "",
    orderKey: seq,
    metadata: {},
    createdAt: "2026-01-01T00:00:00.000Z",
    ...overrides,
  };
}

/** The shape a run leaves behind, in the order it is written. */
function runRecords(runId: string, startKey: number): MessageRecord[] {
  return [
    record({ id: `${runId}-user`, role: "user", orderKey: startKey }),
    // The placeholder is created BEFORE the model answers, so it carries the
    // lowest order key of the three run records — the reordering problem.
    record({
      id: `${runId}-visible`,
      role: "assistant",
      runId,
      orderKey: startKey + 1,
      content: "final answer",
    }),
    record({
      id: `${runId}-internal`,
      role: "assistant",
      runId,
      orderKey: startKey + 2,
      metadata: { internal: true },
      toolCalls: [{ id: "call-1", name: "echo", argumentsJson: "{}" }],
    }),
    record({
      id: `${runId}-tool`,
      role: "tool",
      runId,
      orderKey: startKey + 3,
      toolCallId: "call-1",
      metadata: { internal: true, toolName: "echo" },
    }),
  ];
}

describe("orderMessagesForProvider", () => {
  it("puts the internal assistant turn and its tool results before the visible answer", () => {
    const ordered = orderMessagesForProvider(runRecords("run-1", 1));
    expect(ordered.map((r) => r.id)).toEqual([
      "run-1-user",
      "run-1-internal",
      "run-1-tool",
      "run-1-visible",
    ]);
  });

  it("keeps runs separate — reordering never crosses a run boundary", () => {
    const mixed = [...runRecords("run-1", 1), ...runRecords("run-2", 10)];
    const ordered = orderMessagesForProvider(mixed);
    expect(ordered.map((r) => r.id)).toEqual([
      "run-1-user",
      "run-1-internal",
      "run-1-tool",
      "run-1-visible",
      "run-2-user",
      "run-2-internal",
      "run-2-tool",
      "run-2-visible",
    ]);
  });

  it("orders a tool result before the visible answer even without the internal flag", () => {
    const records = [
      record({ id: "visible", role: "assistant", runId: "r", orderKey: 1 }),
      record({
        id: "internal",
        role: "assistant",
        runId: "r",
        orderKey: 2,
        metadata: { internal: true },
      }),
      record({
        id: "tool",
        role: "tool",
        runId: "r",
        orderKey: 3,
        toolCallId: "c1",
        metadata: {},
      }),
    ];
    // An unflagged tool record must not sort alongside the visible answer: it
    // would land after it, orphaning the tool_call_id on replay.
    expect(orderMessagesForProvider(records).map((r) => r.id)).toEqual([
      "internal",
      "tool",
      "visible",
    ]);
  });

  it("leaves records without a run id in chat order", () => {
    const records = [
      record({ id: "a", role: "user", orderKey: 3 }),
      record({ id: "b", role: "assistant", orderKey: 1 }),
      record({ id: "c", role: "user", orderKey: 2 }),
    ];
    expect(orderMessagesForProvider(records).map((r) => r.id)).toEqual([
      "b",
      "c",
      "a",
    ]);
  });

  it("is stable for equally ranked records and does not mutate its input", () => {
    const records = [
      record({ id: "x", role: "user", orderKey: 1 }),
      record({ id: "y", role: "user", orderKey: 1 }),
      record({ id: "z", role: "user", orderKey: 1 }),
    ];
    const snapshot = records.map((r) => r.id);
    expect(orderMessagesForProvider(records).map((r) => r.id)).toEqual([
      "x",
      "y",
      "z",
    ]);
    expect(records.map((r) => r.id)).toEqual(snapshot);
  });

  it("handles an empty conversation", () => {
    expect(orderMessagesForProvider([])).toEqual([]);
  });
});
