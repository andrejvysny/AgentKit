import type { ImportMessageInput } from "@agentkit/host";
import type { ConversationBranchingOptions } from "./conversation-conformance.js";
import { expectRejectsWithCode } from "./conformance-support.js";

async function fixture(options: ConversationBranchingOptions) {
  const harness = await options.create();
  if (harness.capabilities?.archiveBranches === false) {
    harness.close?.();
    return undefined;
  }
  const conversations = harness.store.conversations;
  const archive = conversations.archiveBranch?.bind(conversations);
  const tree = conversations.listTree?.bind(conversations);
  if (!archive || !tree)
    throw new Error("Archive conformance requires archiveBranch and listTree");
  const chat = await conversations.createChat({});
  const root = await conversations.appendMessage({
    chatId: chat.id,
    role: "user",
    content: "Question",
  });
  const old = await conversations.appendMessage({
    chatId: chat.id,
    role: "assistant",
    content: "quartz old",
    metadata: { audit: { unchanged: true } },
  });
  const child = await conversations.appendMessage({
    chatId: chat.id,
    role: "user",
    content: "quartz child",
  });
  const live = await conversations.appendMessage({
    chatId: chat.id,
    role: "assistant",
    content: "Live answer",
    parentMessageId: root.id,
  });
  return {
    ...harness,
    conversations,
    archive,
    tree,
    chatId: chat.id,
    root,
    old,
    child,
    live,
  };
}

export function describeConversationArchiving(
  options: ConversationBranchingOptions,
): void {
  const { describe, it, expect } = options.test;
  describe("inactive subtree archiving", () => {
    it("hides the whole subtree while preserving records, audit, and sibling indices", async () => {
      const f = await fixture(options);
      if (!f) return;
      try {
        const before = await f.conversations.getMessage(f.old.id);
        expect(await f.archive(f.old.id)).toEqual({ archivedCount: 2 });
        expect(await f.conversations.getMessage(f.old.id)).toEqual({
          ...before,
          archived: true,
        });
        expect((await f.conversations.getMessage(f.child.id))?.archived).toBe(
          true,
        );
        expect((await f.tree(f.chatId)).map((message) => message.id)).toEqual([
          f.root.id,
          f.live.id,
        ]);
        expect(
          (await f.tree(f.chatId, { includeArchived: true })).map(
            (message) => message.id,
          ),
        ).toEqual([f.root.id, f.old.id, f.child.id, f.live.id]);
        expect(
          (await f.conversations.listSiblings(f.live.id)).map((message) => [
            message.id,
            message.branchIndex,
          ]),
        ).toEqual([[f.live.id, 1]]);
        const next = await f.conversations.appendMessage({
          chatId: f.chatId,
          role: "assistant",
          content: "New answer",
          parentMessageId: f.root.id,
        });
        expect(next.branchIndex).toBe(2);
        await f.conversations.appendMessage({
          chatId: f.chatId,
          role: "user",
          content: "Other root",
          parentMessageId: null,
        });
        expect(
          (await f.conversations.activatePath(f.root.id)).map(
            (message) => message.id,
          ),
        ).toEqual([f.root.id, f.live.id]);
        if (f.conversations.searchMessages)
          expect(await f.conversations.searchMessages("quartz")).toEqual([]);
      } finally {
        f.close?.();
      }
    });

    it("refuses active branches and prevents archived branches from being revived", async () => {
      const f = await fixture(options);
      if (!f) return;
      try {
        const before = await f.tree(f.chatId, { includeArchived: true });
        await expectRejectsWithCode(
          f.archive(f.root.id),
          "active_branch_archive",
          expect,
        );
        await expectRejectsWithCode(
          f.archive(f.live.id),
          "active_branch_archive",
          expect,
        );
        expect(await f.tree(f.chatId, { includeArchived: true })).toEqual(
          before,
        );
        await f.archive(f.old.id);
        for (const id of [f.old.id, f.child.id]) {
          await expectRejectsWithCode(
            f.conversations.activatePath(id),
            "not_found",
            expect,
          );
          await expectRejectsWithCode(
            f.conversations.listSiblings(id),
            "not_found",
            expect,
          );
          await expectRejectsWithCode(
            f.conversations.appendMessage({
              chatId: f.chatId,
              role: "user",
              content: "Revive",
              parentMessageId: id,
            }),
            "not_found",
            expect,
          );
          await expectRejectsWithCode(
            f.conversations.forkChat(f.chatId, id),
            "invalid_fork_point",
            expect,
          );
        }
      } finally {
        f.close?.();
      }
    });

    it("rolls archives back with an enclosing transaction on transactional adapters", async () => {
      const f = await fixture(options);
      if (!f) return;
      try {
        if (f.capabilities?.atomicTransactions === false) return;
        const before = await f.tree(f.chatId, { includeArchived: true });
        try {
          await f.store.transaction(async (tx) => {
            await tx.conversations.archiveBranch!(f.old.id);
            throw new Error("rollback");
          });
        } catch (error) {
          if (!(error instanceof Error) || error.message !== "rollback")
            throw error;
        }
        expect(await f.tree(f.chatId, { includeArchived: true })).toEqual(
          before,
        );
      } finally {
        f.close?.();
      }
    });

    it("lists a complete inactive tree beyond normal history windows", async () => {
      const f = await fixture(options);
      if (!f) return;
      try {
        await f.store.transaction(async (tx) => {
          for (let index = 0; index < 205; index++)
            await tx.conversations.appendMessage({
              chatId: f.chatId,
              role: "assistant",
              content: `Branch ${index}`,
              parentMessageId: f.root.id,
            });
        });
        const tree = await f.tree(f.chatId);
        expect(tree.length).toBe(209);
        expect(tree.map((record) => record.orderKey)).toEqual(
          [...tree.map((record) => record.orderKey)].sort(
            (left, right) => left - right,
          ),
        );
      } finally {
        f.close?.();
      }
    });
  });
  describeArchivedImports(options);
}

function describeArchivedImports(options: ConversationBranchingOptions): void {
  const { describe, it, expect } = options.test;
  describe("archived imports", () => {
    it("inherits archive flags, excludes archived children from the live-leaf rule, and keeps audit", async () => {
      const f = await fixture(options);
      if (!f) return;
      try {
        await f.conversations.importConversation({
          chat: { id: "archived-import" },
          messages: [
            { id: "kept", role: "user", content: "Keep", active: true },
            {
              id: "archived",
              parentMessageId: "kept",
              role: "assistant",
              content: "quartz archive",
              active: false,
              archived: true,
            },
            {
              id: "descendant",
              parentMessageId: "archived",
              role: "user",
              content: "quartz descendant",
              active: false,
            },
          ],
        });
        expect((await f.conversations.getMessage("descendant"))?.archived).toBe(
          true,
        );
        expect(
          (await f.tree("archived-import")).map((message) => message.id),
        ).toEqual(["kept"]);
        expect(
          await f.tree("archived-import", { includeArchived: true }),
        ).toHaveLength(3);
        expect(
          (await f.conversations.activatePath("kept")).map(
            (message) => message.id,
          ),
        ).toEqual(["kept"]);
        if (f.conversations.searchMessages)
          expect(
            await f.conversations.searchMessages("quartz", {
              chatId: "archived-import",
            }),
          ).toEqual([]);
      } finally {
        f.close?.();
      }
    });

    it("rejects active archived messages or descendants without writing a chat", async () => {
      const f = await fixture(options);
      if (!f) return;
      try {
        const payloads: ImportMessageInput[][] = [
          [
            {
              id: "root",
              role: "user",
              content: "Archived active",
              active: true,
              archived: true,
            },
          ],
          [
            {
              id: "root",
              role: "user",
              content: "Archived",
              active: false,
              archived: true,
            },
            {
              id: "child",
              parentMessageId: "root",
              role: "assistant",
              content: "Active",
              active: true,
            },
          ],
        ];
        for (const [index, messages] of payloads.entries()) {
          const chatId = `invalid-archive-${index}`;
          await expectRejectsWithCode(
            f.conversations.importConversation({
              chat: { id: chatId },
              messages,
            }),
            "invalid_import",
            expect,
          );
          expect(await f.conversations.getChat(chatId)).toBeNull();
        }
      } finally {
        f.close?.();
      }
    });
  });
}
