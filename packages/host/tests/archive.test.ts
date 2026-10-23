import { describe, expect, it } from "bun:test";
import { archiveSubtreeOf, type MessageRecord } from "../src/index.js";
import { anchoredHistory } from "../src/turn/anchored-history.js";
import { createHarness } from "./fakes.js";

function record(
  id: string,
  overrides: Partial<MessageRecord> = {},
): MessageRecord {
  return {
    id,
    chatId: "chat",
    role: "assistant",
    content: "Audit",
    orderKey: 1,
    depth: 0,
    branchIndex: 0,
    active: false,
    metadata: {},
    createdAt: "2026-10-23T00:00:00Z",
    ...overrides,
  };
}

describe("archive subtree validation", () => {
  it("refuses an active descendant even when the target itself is inactive", () => {
    const records = [
      record("parent"),
      record("child", { parentMessageId: "parent", active: true }),
    ];
    const before = structuredClone(records);
    expect(() => archiveSubtreeOf(records, "parent")).toThrow(
      "Cannot archive active branch. Activate a different branch first.",
    );
    expect(records).toEqual(before);
  });

  it("never mutates records and does not loop on corrupt parent cycles", () => {
    const records = [
      record("parent", { parentMessageId: "child" }),
      record("child", { parentMessageId: "parent" }),
    ];
    expect(
      archiveSubtreeOf(records, "parent").map((message) => message.id),
    ).toEqual(["parent", "child"]);
    expect(records.every((message) => message.archived === undefined)).toBe(
      true,
    );
  });

  it("fails closed when a run's anchor, ancestor, or own tool record was archived", async () => {
    for (const archived of ["root", "answer", "tool"] as const) {
      const { store } = createHarness();
      const chat = await store.conversations.createChat({});
      await store.conversations.appendMessage({
        chatId: chat.id,
        id: "root",
        role: "user",
        content: "Question",
      });
      await store.conversations.appendMessage({
        chatId: chat.id,
        id: "answer",
        role: "assistant",
        runId: "run",
        content: "",
        metadata: { placeholder: true },
      });
      await store.conversations.appendMessage({
        chatId: chat.id,
        id: "tool",
        role: "tool",
        runId: "run",
        content: "Result",
        metadata: { internal: true },
        parentMessageId: "answer",
        activate: false,
      });
      store.conversations.messages.find(
        (message) => message.id === archived,
      )!.archived = true;
      await expect(
        anchoredHistory(store.conversations, chat.id, "answer", 200),
      ).rejects.toMatchObject({ code: "invalid_history_anchor" });
    }
  });
});
