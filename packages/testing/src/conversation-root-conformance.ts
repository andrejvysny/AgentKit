import type { ConversationBranchingOptions } from "./conversation-conformance.js";
import { expectRejectsWithCode } from "./conformance-support.js";

export function describeRootBranching(
  options: ConversationBranchingOptions,
): void {
  const {
    create,
    test: { describe, it, expect },
  } = options;
  describe("explicit root branching", () => {
    it("keeps null distinct from omitted parents and preserves old roots", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const oldRoot = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "Original",
        });
        const oldAnswer = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "Old answer",
        });
        const root = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "Edited root",
          parentMessageId: null,
        });
        expect(root.parentMessageId).toBe(undefined);
        expect(root.depth).toBe(0);
        expect(root.branchIndex).toBe(1);
        expect(
          (await store.conversations.listMessages(chat.id)).map(
            (message) => message.id,
          ),
        ).toEqual([root.id]);
        const answer = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "New answer",
        });
        expect(answer.parentMessageId).toBe(root.id);
        expect(answer.depth).toBe(1);
        const siblings = await store.conversations.listSiblings(root.id);
        expect(siblings.map((message) => message.id)).toEqual([
          oldRoot.id,
          root.id,
        ]);
        expect(siblings.map((message) => message.active)).toEqual([
          false,
          true,
        ]);
        await store.conversations.activatePath(oldRoot.id);
        expect(
          (await store.conversations.listMessages(chat.id)).map(
            (message) => message.id,
          ),
        ).toEqual([oldRoot.id, oldAnswer.id]);
        expect((await store.conversations.getMessage(root.id))?.content).toBe(
          "Edited root",
        );
      } finally {
        close?.();
      }
    });

    it("refuses a chain append with a null parent before writing anything", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        await expectRejectsWithCode(
          store.conversations.appendMessage({
            chatId: chat.id,
            role: "assistant",
            content: "No parent",
            parentMessageId: null,
            activate: false,
          }),
          "invalid_append",
          expect,
        );
        expect(await store.conversations.listMessages(chat.id)).toEqual([]);
      } finally {
        close?.();
      }
    });
  });
}
