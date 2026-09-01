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
    // Tree fields at their degenerate-linear defaults: this suite is about
    // provider ordering WITHIN a run, which branching does not touch.
    depth: seq - 1,
    branchIndex: 0,
    active: true,
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

/**
 * What a run with TWO tool-calling passes leaves behind — the shape the
 * correction harness produces, since its write-back tells the model to call its
 * tools again on the SAME run id.
 */
function twoPassRecords(runId: string): MessageRecord[] {
  return [
    record({ id: "user", role: "user", orderKey: 1 }),
    record({
      id: "visible",
      role: "assistant",
      runId,
      orderKey: 2,
      content: "corrected answer",
    }),
    record({
      id: "pass1-assistant",
      role: "assistant",
      runId,
      orderKey: 3,
      metadata: { internal: true },
      toolCalls: [{ id: "call-1", name: "echo", argumentsJson: "{}" }],
    }),
    record({
      id: "pass1-tool",
      role: "tool",
      runId,
      orderKey: 4,
      toolCallId: "call-1",
      metadata: { internal: true, toolName: "echo" },
    }),
    record({
      id: "write-back",
      role: "user",
      runId,
      orderKey: 5,
      metadata: { internal: true, correctionPass: 1 },
    }),
    record({
      id: "pass2-assistant",
      role: "assistant",
      runId,
      orderKey: 6,
      metadata: { internal: true },
      toolCalls: [{ id: "call-2", name: "echo", argumentsJson: "{}" }],
    }),
    record({
      id: "pass2-tool",
      role: "tool",
      runId,
      orderKey: 7,
      toolCallId: "call-2",
      metadata: { internal: true, toolName: "echo" },
    }),
  ];
}

describe("orderMessagesForProvider — a run with two tool-calling passes", () => {
  it("keeps each pass next to its own tool results", () => {
    // Bucketing by KIND would give assistant, assistant, tool, tool: two
    // tool-calling turns back to back, the first one unanswered, which every
    // provider rejects outright.
    expect(
      orderMessagesForProvider(twoPassRecords("run-1")).map((r) => r.id),
    ).toEqual([
      "user",
      "pass1-assistant",
      "pass1-tool",
      "pass2-assistant",
      "pass2-tool",
      "visible",
      "write-back",
    ]);
  });

  it("groups by linkage, not by adjacency, when results land out of order", () => {
    const records = twoPassRecords("run-1");
    // The second pass's result written before the first pass's — a projection
    // that fell behind, or an import that named its own order.
    const swapped = [
      ...records.filter((r) => r.id !== "pass2-tool"),
      {
        ...(records.find((r) => r.id === "pass2-tool") as MessageRecord),
        orderKey: 3.5,
      },
    ];
    expect(orderMessagesForProvider(swapped).map((r) => r.id)).toEqual([
      "user",
      "pass1-assistant",
      "pass1-tool",
      "pass2-assistant",
      "pass2-tool",
      "visible",
      "write-back",
    ]);
  });

  it("leaves a result no turn in the run declared ahead of the answer", () => {
    const records = [
      ...twoPassRecords("run-1"),
      record({
        id: "orphan-tool",
        role: "tool",
        runId: "run-1",
        orderKey: 8,
        toolCallId: "call-99",
        metadata: { internal: true },
      }),
    ];
    // Nothing claims it, so it keeps chat order — still ahead of the visible
    // answer, where the caller drops it as the orphan it is.
    expect(orderMessagesForProvider(records).map((r) => r.id)).toEqual([
      "user",
      "pass1-assistant",
      "pass1-tool",
      "pass2-assistant",
      "pass2-tool",
      "orphan-tool",
      "visible",
      "write-back",
    ]);
  });

  it("does not let a second turn re-claim the first turn's results", () => {
    // A malformed run that declared the same id twice: one result, one owner.
    const records = [
      record({
        id: "a1",
        role: "assistant",
        runId: "r",
        orderKey: 1,
        metadata: { internal: true },
        toolCalls: [{ id: "dup", name: "echo", argumentsJson: "{}" }],
      }),
      record({
        id: "a2",
        role: "assistant",
        runId: "r",
        orderKey: 2,
        metadata: { internal: true },
        toolCalls: [{ id: "dup", name: "echo", argumentsJson: "{}" }],
      }),
      record({
        id: "t",
        role: "tool",
        runId: "r",
        orderKey: 3,
        toolCallId: "dup",
        metadata: { internal: true },
      }),
    ];
    expect(orderMessagesForProvider(records).map((r) => r.id)).toEqual([
      "a1",
      "t",
      "a2",
    ]);
  });
});
