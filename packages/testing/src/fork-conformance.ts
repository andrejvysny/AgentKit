// Chat forking: copying a prefix of one conversation's active path into a new
// chat, in one transaction.
//
// Its own module rather than more of `conversation-conformance.ts`, which had
// grown past the size this repo keeps files at. The seam is real — nothing here
// shares a fixture with the branch-switching tests, and a fork is graded on a
// different question: not "which path is live?" but "what did the copy keep,
// what did it drop, and can it be undone?".
//
// FRAMEWORK-NEUTRAL, same rules as the rest of this package: no runner import,
// every `@agentkit/host` import is `import type`, and error assertions match on
// the `code` string rather than on `instanceof`.
import type { AiContentPart } from "@agentkit/contracts";
import type { AssistantStore, MessageRecord } from "@agentkit/host";
import {
  expectRejects,
  expectRejectsWithCode,
  type AssistantStoreConformanceHarness,
  type AssistantStoreConformanceTestApi,
} from "./conformance-support.js";

export interface ConversationForkOptions {
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** Ids of a record list, in the order the store returned them. */
function ids(records: readonly MessageRecord[]): string[] {
  return records.map((record) => record.id);
}

/** Text, an inlined image, and a host attachment ref — twin of the one in `conversation-conformance.ts`. */
function multimodalBody(): AiContentPart[] {
  return [
    { type: "text", text: "what is in this picture?" },
    {
      type: "image",
      source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
      detail: "high",
    },
    { type: "image", source: { kind: "ref", ref: "blob:sha256-abc123" } },
  ];
}

/**
 * `u1 → a1` with a second answer `a2` branched under `u1` and then abandoned —
 * the smallest chat with a message that exists, belongs to the chat, and is NOT
 * a legal fork point.
 */
async function seedBranched(store: AssistantStore, chatId: string) {
  const u1 = await store.conversations.appendMessage({
    chatId,
    role: "user",
    content: "u1",
  });
  const a1 = await store.conversations.appendMessage({
    chatId,
    role: "assistant",
    content: "a1",
  });
  const a2 = await store.conversations.appendMessage({
    chatId,
    role: "assistant",
    content: "a2",
    parentMessageId: u1.id,
  });
  await store.conversations.activatePath(a1.id);
  return { u1, a1, a2 };
}

export function describeConversationForking(
  options: ConversationForkOptions,
): void {
  const { create, test } = options;
  const { describe, it, expect } = test;

  describe("conversation forking", () => {
    it("forkChat copies the active-path prefix: fresh ids, remapped parents, no runId, no placeholder, internals kept", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({
          title: "Original",
          metadata: { label: "keep-me" },
        });
        const u1 = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "question",
        });
        // A still-streaming answer, mid-path: it must be dropped AND the copies
        // either side of it must re-link across the gap.
        const pending = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "",
          runId: "run-1",
          metadata: { placeholder: true },
        });
        const internal = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "",
          runId: "run-1",
          toolCalls: [{ id: "call-1", name: "echo", argumentsJson: "{}" }],
          metadata: { internal: true },
        });
        const toolResult = await store.conversations.appendMessage({
          chatId: source.id,
          role: "tool",
          content: '{"ok":true}',
          runId: "run-1",
          toolCallId: "call-1",
          modelResultJson: '{"ok":true}',
          metadata: { internal: true, toolName: "echo" },
        });
        const answer = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "the answer",
          runId: "run-1",
          metadata: { placeholder: false },
        });
        // Beyond the fork point: must not be copied.
        await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "and another thing",
        });

        const forked = await store.conversations.forkChat(source.id, answer.id);
        expect(forked.chat.id).not.toBe(source.id);
        expect(forked.chat.title).toBe("Fork of Original");
        expect(forked.chat.metadata).toEqual({ label: "keep-me" });

        const copies = forked.messages;
        expect(copies.map((m) => m.content)).toEqual([
          "question",
          "",
          '{"ok":true}',
          "the answer",
        ]);
        // Fresh ids, and none of the sources reused.
        const sourceIds = new Set([
          u1.id,
          pending.id,
          internal.id,
          toolResult.id,
          answer.id,
        ]);
        for (const copy of copies) expect(sourceIds.has(copy.id)).toBe(false);
        // Re-linked across the dropped placeholder, root parented to nothing.
        expect(copies.map((m) => m.parentMessageId)).toEqual([
          undefined,
          copies[0]?.id,
          copies[1]?.id,
          copies[2]?.id,
        ]);
        expect(copies.map((m) => m.depth)).toEqual([0, 1, 2, 3]);
        expect(copies.map((m) => m.branchIndex)).toEqual([0, 0, 0, 0]);
        expect(copies.map((m) => m.active)).toEqual([true, true, true, true]);
        expect(copies.map((m) => m.runId)).toEqual([
          undefined,
          undefined,
          undefined,
          undefined,
        ]);
        expect(copies.map((m) => m.chatId)).toEqual([
          forked.chat.id,
          forked.chat.id,
          forked.chat.id,
          forked.chat.id,
        ]);
        // Replay-only records survive, with their flags; the streaming flag does
        // not survive on the one message that carried a false one.
        expect(copies[1]?.metadata).toEqual({ internal: true });
        expect(copies[2]?.metadata).toEqual({
          internal: true,
          toolName: "echo",
        });
        expect(copies[1]?.toolCalls).toEqual([
          { id: "call-1", name: "echo", argumentsJson: "{}" },
        ]);
        expect(copies[2]?.toolCallId).toBe("call-1");
        expect(copies[2]?.modelResultJson).toBe('{"ok":true}');
        expect("placeholder" in (copies[3]?.metadata ?? {})).toBe(false);

        // And the fork reads back as a conversation.
        const listed = await store.conversations.listMessages(forked.chat.id);
        expect(ids(listed)).toEqual(ids(copies));
      } finally {
        close?.();
      }
    });

    it("forkChat stores the copy in PROVIDER order, not in the order the source was written", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({});
        const u1 = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "question",
        });
        // The visible answer is created FIRST, as an empty placeholder, the
        // moment the user hits send — and only filled in once the turn is
        // over. So in a real chat it carries a LOWER `orderKey` than the
        // internal assistant turn and the tool result that produced it.
        const answer = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "",
          runId: "run-1",
          metadata: { placeholder: true },
        });
        const internal = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "",
          runId: "run-1",
          toolCalls: [{ id: "call-1", name: "echo", argumentsJson: "{}" }],
          metadata: { internal: true },
        });
        const toolResult = await store.conversations.appendMessage({
          chatId: source.id,
          role: "tool",
          content: '{"ok":true}',
          runId: "run-1",
          toolCallId: "call-1",
          modelResultJson: '{"ok":true}',
          metadata: { internal: true, toolName: "echo" },
        });
        await store.conversations.updateMessage(answer.id, {
          content: "the answer",
          metadata: { placeholder: false },
        });
        expect(answer.orderKey < internal.orderKey).toBe(true);

        const forked = await store.conversations.forkChat(
          source.id,
          toolResult.id,
        );
        // A fork drops `runId`, which is the ONLY thing that tells a replay how
        // to put those three back in the order a provider accepts — so if the
        // copy were written in source order it would replay a tool result
        // before the turn that asked for it, forever. The repair therefore
        // happens once, here, and the fork's STORED order is provider order.
        expect(forked.messages.map((m) => m.role)).toEqual([
          "user",
          "assistant",
          "tool",
          "assistant",
        ]);
        expect(forked.messages.map((m) => m.content)).toEqual([
          "question",
          "",
          '{"ok":true}',
          "the answer",
        ]);
        expect(forked.messages[1]?.toolCalls).toEqual([
          { id: "call-1", name: "echo", argumentsJson: "{}" },
        ]);
        // Renumbered on the repaired sequence, not on the source's: depth,
        // orderKey and the parent chain all agree with the order above.
        expect(forked.messages.map((m) => m.depth)).toEqual([0, 1, 2, 3]);
        expect(forked.messages.map((m) => m.parentMessageId)).toEqual([
          undefined,
          forked.messages[0]?.id,
          forked.messages[1]?.id,
          forked.messages[2]?.id,
        ]);
        const keys = forked.messages.map((m) => m.orderKey);
        expect(
          keys.every((key, i) => i === 0 || key > (keys[i - 1] ?? 0)),
        ).toBe(true);
        // And that is what the fork reads back as, too.
        expect(
          (await store.conversations.listMessages(forked.chat.id)).map(
            (m) => m.content,
          ),
        ).toEqual(["question", "", '{"ok":true}', "the answer"]);
        expect(u1.id).not.toBe(forked.messages[0]?.id);
      } finally {
        close?.();
      }
    });

    it("forkChat keeps a chat with no title untitled rather than inventing one", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({});
        const m1 = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "hi",
        });
        const forked = await store.conversations.forkChat(source.id, m1.id);
        expect(forked.chat.title).toBe(undefined);
      } finally {
        close?.();
      }
    });

    it("forkChat rejects an unknown, cross-chat or off-path fork point, and writes nothing", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({});
        const t = await seedBranched(store, source.id);
        const other = await store.conversations.createChat({});
        const elsewhere = await store.conversations.appendMessage({
          chatId: other.id,
          role: "user",
          content: "elsewhere",
        });
        const chatsBefore = (await store.conversations.listChats()).length;

        await expectRejectsWithCode(
          store.conversations.forkChat(source.id, "msg-does-not-exist"),
          "invalid_fork_point",
          expect,
        );
        await expectRejectsWithCode(
          store.conversations.forkChat(source.id, elsewhere.id),
          "invalid_fork_point",
          expect,
        );
        // Real message, real chat, wrong branch: a2 was branched and then
        // abandoned. This is the case a store that only checked existence would
        // happily fork.
        await expectRejectsWithCode(
          store.conversations.forkChat(source.id, t.a2.id),
          "invalid_fork_point",
          expect,
        );
        await expectRejectsWithCode(
          store.conversations.forkChat("chat-does-not-exist", t.a1.id),
          "not_found",
          expect,
        );
        expect((await store.conversations.listChats()).length).toBe(
          chatsBefore,
        );
      } finally {
        close?.();
      }
    });

    it("a fork and its source are independent in both directions", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({ title: "S" });
        const u1 = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "q",
        });
        const a1 = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "a",
        });
        const forked = await store.conversations.forkChat(source.id, a1.id);

        await store.conversations.appendMessage({
          chatId: forked.chat.id,
          role: "user",
          content: "only in the fork",
        });
        expect(ids(await store.conversations.listMessages(source.id))).toEqual([
          u1.id,
          a1.id,
        ]);

        await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "only in the source",
        });
        const forkPath = await store.conversations.listMessages(forked.chat.id);
        expect(forkPath.map((m) => m.content)).toEqual([
          "q",
          "a",
          "only in the fork",
        ]);

        // Structural independence too: branching the fork must not disturb the
        // source's active path, even though the copies descend from it.
        await store.conversations.appendMessage({
          chatId: forked.chat.id,
          role: "assistant",
          content: "fork branch",
          parentMessageId: forked.messages[0]?.id ?? "",
        });
        expect(
          (await store.conversations.listMessages(source.id)).map(
            (m) => m.content,
          ),
        ).toEqual(["q", "a", "only in the source"]);
      } finally {
        close?.();
      }
    });

    it("forkChat rolls back with the transaction that wrapped it", async () => {
      const { store, capabilities, close } = await create();
      try {
        // Adapters that document no rollback opt out, same as the store-wide
        // atomicity probe. What is being graded here is that `forkChat`'s writes
        // JOIN the ambient transaction instead of escaping it — a fork that
        // opened its own would survive this rollback and leave an orphan chat.
        if (capabilities?.atomicTransactions === false) return;
        const source = await store.conversations.createChat({});
        const m1 = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "hi",
        });
        let forkedChatId = "";
        await expectRejects(
          store.transaction(async (tx) => {
            const result = await tx.conversations.forkChat(source.id, m1.id);
            forkedChatId = result.chat.id;
            throw new Error("boom");
          }),
          expect,
        );
        expect(forkedChatId).not.toBe("");
        expect(await store.conversations.getChat(forkedChatId)).toBeNull();
        expect(
          (await store.conversations.listMessages(forkedChatId)).length,
        ).toBe(0);
        // The source is untouched by the failed fork.
        expect(ids(await store.conversations.listMessages(source.id))).toEqual([
          m1.id,
        ]);
      } finally {
        close?.();
      }
    });

    it("forkChat copies content parts byte-exact, refs included", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({});
        const parts = multimodalBody();
        const question = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: parts,
        });
        const answer = await store.conversations.appendMessage({
          chatId: source.id,
          role: "assistant",
          content: "a picture of a cat",
        });

        const fork = await store.conversations.forkChat(source.id, answer.id);
        // Both halves: what forkChat returned, and what the new chat reads back
        // as — a fork that serialized correctly on the way out and lost the
        // format tag on the way in would pass only the first.
        expect(fork.messages[0]?.content).toEqual(parts);
        expect(fork.messages[1]?.content).toBe("a picture of a cat");
        const copied = await store.conversations.listMessages(fork.chat.id);
        expect(copied[0]?.content).toEqual(parts);
        expect(copied[1]?.content).toBe("a picture of a cat");

        // A ref survives the copy in particular. It has to: the fork replays to
        // a provider like any other chat, and `TurnRunner` resolves the ref
        // again on the fork's next turn.
        const forkedParts = copied[0]?.content as AiContentPart[];
        expect(forkedParts[2]).toEqual({
          type: "image",
          source: { kind: "ref", ref: "blob:sha256-abc123" },
        });

        // And the copy is independent of the original in both directions — the
        // fork's whole point, applied to the one field that is now a mutable
        // object graph rather than a string.
        await store.conversations.updateMessage(question.id, {
          content: "the source lost its attachments",
        });
        expect(
          (await store.conversations.listMessages(fork.chat.id))[0]?.content,
        ).toEqual(parts);
      } finally {
        close?.();
      }
    });
  });
}
