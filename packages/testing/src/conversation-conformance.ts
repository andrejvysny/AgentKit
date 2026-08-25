// The conversation half of the store-conformance contract: a chat is a tree, and
// `listMessages` reports one path through it. Forking that path into a new chat
// is graded next door, in `fork-conformance.ts`.
//
// Split out of `store-conformance.ts` rather than appended to it — that file was
// already long, and these tests share no fixtures with the queue and proposal
// sections. Called from the main suite's `describe`, so an adapter still opts in
// exactly once.
//
// FRAMEWORK-NEUTRAL, same rules as the rest of this package: no runner import,
// every `@agentkit/host` import is `import type`, and error assertions match on
// the `code` string rather than on `instanceof`.
import type { AssistantStore, MessageRecord } from "@agentkit/host";
import {
  expectRejectsWithCode,
  type AssistantStoreConformanceHarness,
  type AssistantStoreConformanceTestApi,
} from "./conformance-support.js";

export interface ConversationBranchingOptions {
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** Ids of a record list, in the order the store returned them. */
function ids(records: readonly MessageRecord[]): string[] {
  return records.map((record) => record.id);
}

/**
 * A three-level tree with two branches under the root and two answers under a
 * mid-level node — the smallest shape that exercises every rule in
 * `activationSetOf` at once.
 *
 * ```
 * u1                       (root)
 * ├── a1   branchIndex 0
 * │   └── u2
 * │       ├── a2  branchIndex 0
 * │       └── a4  branchIndex 1   ← left active
 * └── a3   branchIndex 1
 *     └── u3
 * ```
 *
 * Left active on `u1 → a1 → u2 → a4` (the state the last append leaves behind).
 * Both descent rules are observable from there: re-activating a node ALREADY on
 * the path must keep `a4` (prefer the active child, where the lowest index would
 * pick `a2`), and re-activating `a1` after a switch away has cleared its whole
 * subtree must pick `a2` (the lowest-index fallback, where a store that
 * remembered would pick `a4`).
 */
async function buildTree(store: AssistantStore, chatId: string) {
  const append = (
    content: string,
    role: MessageRecord["role"],
    parentMessageId?: string,
  ) =>
    store.conversations.appendMessage({
      chatId,
      role,
      content,
      ...(parentMessageId === undefined ? {} : { parentMessageId }),
    });

  const u1 = await append("u1", "user");
  const a1 = await append("a1", "assistant");
  const u2 = await append("u2", "user");
  const a2 = await append("a2", "assistant");
  // Branch under the root: a second answer to the very first question.
  const a3 = await append("a3", "assistant", u1.id);
  const u3 = await append("u3", "user");
  // Branch under u2, taken while u2 was NOT on the path (a3's branch is).
  const a4 = await append("a4", "assistant", u2.id);
  return { u1, a1, u2, a2, a3, u3, a4 };
}

export function describeConversationBranching(
  options: ConversationBranchingOptions,
): void {
  const { create, test } = options;
  const { describe, it, expect } = test;

  describe("conversation branching", () => {
    it("appends a linear chain: parent links, rising depth, branchIndex 0, all active", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const m1 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "hi",
        });
        const m2 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "hello",
        });
        const m3 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "again",
        });

        expect(m1.parentMessageId).toBe(undefined);
        expect(m2.parentMessageId).toBe(m1.id);
        expect(m3.parentMessageId).toBe(m2.id);
        expect([m1.depth, m2.depth, m3.depth]).toEqual([0, 1, 2]);
        expect([m1.branchIndex, m2.branchIndex, m3.branchIndex]).toEqual([
          0, 0, 0,
        ]);
        expect([m1.active, m2.active, m3.active]).toEqual([true, true, true]);
        const listed = await store.conversations.listMessages(chat.id);
        expect(ids(listed)).toEqual([m1.id, m2.id, m3.id]);
      } finally {
        close?.();
      }
    });

    it("rejects an append whose named parent is unknown or in another chat", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const other = await store.conversations.createChat({});
        const elsewhere = await store.conversations.appendMessage({
          chatId: other.id,
          role: "user",
          content: "elsewhere",
        });
        await expectRejectsWithCode(
          store.conversations.appendMessage({
            chatId: chat.id,
            role: "user",
            content: "orphan",
            parentMessageId: "msg-does-not-exist",
          }),
          "not_found",
          expect,
        );
        await expectRejectsWithCode(
          store.conversations.appendMessage({
            chatId: chat.id,
            role: "user",
            content: "cross-chat",
            parentMessageId: elsewhere.id,
          }),
          "not_found",
          expect,
        );
        expect((await store.conversations.listMessages(chat.id)).length).toBe(
          0,
        );
      } finally {
        close?.();
      }
    });

    it("a branching append takes the next branchIndex and switches the active path in one write", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const u1 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "q",
        });
        const a1 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "first answer",
        });
        const a2 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "second answer",
          parentMessageId: u1.id,
        });

        expect(a1.branchIndex).toBe(0);
        expect(a2.branchIndex).toBe(1);
        expect(a2.depth).toBe(1);
        expect(a2.active).toBe(true);
        // The switch is part of the append, not a follow-up: the very next read
        // reports the new branch, and the answer it replaced is gone from it.
        const listed = await store.conversations.listMessages(chat.id);
        expect(ids(listed)).toEqual([u1.id, a2.id]);
      } finally {
        close?.();
      }
    });

    it("listMessages reports only the active path, in (depth, orderKey) order, across switches", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const t = await buildTree(store, chat.id);
        expect(ids(await store.conversations.listMessages(chat.id))).toEqual([
          t.u1.id,
          t.a1.id,
          t.u2.id,
          t.a4.id,
        ]);

        await store.conversations.activatePath(t.a3.id);
        expect(ids(await store.conversations.listMessages(chat.id))).toEqual([
          t.u1.id,
          t.a3.id,
          t.u3.id,
        ]);

        // `limit` still means "the most recent N", and `afterOrderKey` still
        // pages forward — both now over the path rather than the whole chat.
        const tail = await store.conversations.listMessages(chat.id, {
          limit: 2,
        });
        expect(ids(tail)).toEqual([t.a3.id, t.u3.id]);
        const after = await store.conversations.listMessages(chat.id, {
          afterOrderKey: t.u1.orderKey,
        });
        expect(ids(after)).toEqual([t.a3.id, t.u3.id]);
      } finally {
        close?.();
      }
    });

    it("listSiblings returns same-parent messages including self, branchIndex ascending", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const t = await buildTree(store, chat.id);

        expect(ids(await store.conversations.listSiblings(t.a1.id))).toEqual([
          t.a1.id,
          t.a3.id,
        ]);
        // Asked from the OTHER sibling: the list is a property of the parent,
        // not of who asked.
        expect(ids(await store.conversations.listSiblings(t.a3.id))).toEqual([
          t.a1.id,
          t.a3.id,
        ]);
        expect(ids(await store.conversations.listSiblings(t.a4.id))).toEqual([
          t.a2.id,
          t.a4.id,
        ]);
        // A root's siblings are the chat's roots — here, just itself.
        expect(ids(await store.conversations.listSiblings(t.u1.id))).toEqual([
          t.u1.id,
        ]);
        await expectRejectsWithCode(
          store.conversations.listSiblings("msg-does-not-exist"),
          "not_found",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("activatePath activates ancestors plus a descent, preferring an active child and otherwise the lowest branchIndex", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const t = await buildTree(store, chat.id);
        const activeIds = async () =>
          ids(await store.conversations.listMessages(chat.id));

        const branchA4 = [t.u1.id, t.a1.id, t.u2.id, t.a4.id];
        const branchA2 = [t.u1.id, t.a1.id, t.u2.id, t.a2.id];
        expect(await activeIds()).toEqual(branchA4);

        // PREFER THE ACTIVE CHILD. Re-activating a node already ON the path must
        // leave the path alone: the descent from a1 reaches u2, whose children
        // are a2 (index 0, inactive) and a4 (index 1, ACTIVE). Lowest-index
        // would answer a2 here, so this is the assertion that tells the two
        // rules apart — and the reason activating an ancestor is not a
        // destructive "reset to the first answer".
        await store.conversations.activatePath(t.a1.id);
        expect(await activeIds()).toEqual(branchA4);
        // Same, from the root, where the first branch point has two children.
        await store.conversations.activatePath(t.u1.id);
        expect(await activeIds()).toEqual(branchA4);

        // Switch to the other root branch. a3's only child was never active, so
        // the descent takes the lowest branchIndex — u3.
        await store.conversations.activatePath(t.a3.id);
        expect(await activeIds()).toEqual([t.u1.id, t.a3.id, t.u3.id]);

        // LOWEST BRANCH INDEX. Back to a1: the switch away cleared its whole
        // subtree, so nothing under u2 is active any more and the descent falls
        // back to a2 — NOT to a4, which is where it was before. The active flag
        // is the path, not a per-node bookmark that survives leaving.
        await store.conversations.activatePath(t.a1.id);
        expect(await activeIds()).toEqual(branchA2);

        // Naming a leaf directly is the way back to the other answer.
        await store.conversations.activatePath(t.a4.id);
        expect(await activeIds()).toEqual(branchA4);

        // Activating a MIDDLE node re-descends from it; it does not truncate.
        await store.conversations.activatePath(t.u2.id);
        expect(await activeIds()).toEqual(branchA4);
        await expectRejectsWithCode(
          store.conversations.activatePath("msg-does-not-exist"),
          "not_found",
          expect,
        );
      } finally {
        close?.();
      }
    });
  });
}
