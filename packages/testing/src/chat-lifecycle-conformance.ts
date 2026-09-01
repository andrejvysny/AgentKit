// The lifecycle half of the conversation contract: a chat can be renamed,
// archived, listed by id, paged BACKWARDS, and deleted — with everything the
// host holds about it.
//
// Split out of `store-conformance.ts` for the same reason the branching and
// forking sections were: these tests share no fixtures with the queue and
// proposal sections, and the file they would otherwise be appended to is
// already long. Called from the main suite's `describe`, so an adapter still
// opts in exactly once.
//
// FRAMEWORK-NEUTRAL, same rules as the rest of this package: no runner import,
// every `@agentkit/host` import is `import type`, and error assertions match on
// the `code` string rather than on `instanceof`.
import type {
  AssistantStore,
  ClaimedTask,
  CreateTaskInput,
  TaskStatus,
} from "@agentkit/host";
import {
  expectRejectsWithCode,
  type AssistantStoreConformanceHarness,
  type AssistantStoreConformanceTestApi,
} from "./conformance-support.js";
import { createTestEventStamper } from "./stamp.js";

export interface ChatLifecycleOptions {
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** A queued task in a chat's scope — what `deleteByScope` has to sweep up. */
function taskInScope(scopeId: string, taskId: string): CreateTaskInput {
  return {
    taskId,
    kind: "chat.turn",
    scopeId,
    payload: { model: "test-model" },
  };
}

/** Appends `count` linear messages and returns them in append order. */
async function appendChain(
  store: AssistantStore,
  chatId: string,
  count: number,
) {
  const messages = [];
  for (let index = 0; index < count; index += 1) {
    messages.push(
      await store.conversations.appendMessage({
        chatId,
        role: index % 2 === 0 ? "user" : "assistant",
        content: `m${index + 1}`,
      }),
    );
  }
  return messages;
}

export function describeChatLifecycle(options: ChatLifecycleOptions): void {
  const { create, test } = options;
  const { describe, it, expect } = test;

  describe("chat lifecycle", () => {
    it("updateChat renames, REPLACES metadata, and round-trips the archived flag", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({
          title: "Original",
          metadata: { pinned: true, colour: "red" },
        });
        // A fresh chat is never archived — the default is a promise, not an
        // accident of whichever adapter is underneath.
        expect(chat.archived).toBe(false);

        const renamed = await store.conversations.updateChat(chat.id, {
          title: "Renamed",
        });
        expect(renamed.title).toBe("Renamed");
        // Metadata untouched by a patch that did not mention it.
        expect(renamed.metadata).toEqual({ pinned: true, colour: "red" });

        // REPLACES, exactly as updateMessage does: a merge would make
        // "unset this flag" unexpressible.
        const rebagged = await store.conversations.updateChat(chat.id, {
          metadata: { colour: "blue" },
        });
        expect(rebagged.metadata).toEqual({ colour: "blue" });
        expect(rebagged.title).toBe("Renamed");

        const archived = await store.conversations.updateChat(chat.id, {
          archived: true,
        });
        expect(archived.archived).toBe(true);
        // Read back through a different method: the flag is persisted, not
        // just returned.
        expect((await store.conversations.getChat(chat.id))?.archived).toBe(
          true,
        );
        const restored = await store.conversations.updateChat(chat.id, {
          archived: false,
        });
        expect(restored.archived).toBe(false);
        expect(restored.title).toBe("Renamed");
        expect(restored.metadata).toEqual({ colour: "blue" });
      } finally {
        close?.();
      }
    });

    it("updateChat rejects an unknown chat rather than creating one", async () => {
      const { store, close } = await create();
      try {
        await expectRejectsWithCode(
          store.conversations.updateChat("chat-nope", { title: "x" }),
          "not_found",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("listChats hides archived chats by default, shows them on request, and resolves them by id regardless", async () => {
      const { store, close } = await create();
      try {
        const live = await store.conversations.createChat({ title: "Live" });
        const filed = await store.conversations.createChat({ title: "Filed" });
        await store.conversations.updateChat(filed.id, { archived: true });

        const browsed = await store.conversations.listChats();
        expect(browsed.map((c) => c.id)).toEqual([live.id]);

        const everything = await store.conversations.listChats({
          includeArchived: true,
        });
        expect([...everything.map((c) => c.id)].sort()).toEqual(
          [live.id, filed.id].sort(),
        );

        // An explicit id is not a browse: naming an archived chat resolves it.
        const named = await store.conversations.listChats({ ids: [filed.id] });
        expect(named.map((c) => c.id)).toEqual([filed.id]);
        expect(named[0]?.archived).toBe(true);

        // A batch fetch of both, still ordered and paged like the listing.
        const both = await store.conversations.listChats({
          ids: [filed.id, live.id],
        });
        expect([...both.map((c) => c.id)].sort()).toEqual(
          [live.id, filed.id].sort(),
        );
        expect(
          (await store.conversations.listChats({ ids: [filed.id], limit: 1 }))
            .length,
        ).toBe(1);

        // Ids that name nothing simply do not appear; an EMPTY list is "none
        // of these", never "no filter".
        expect(
          (await store.conversations.listChats({ ids: ["chat-nope"] })).length,
        ).toBe(0);
        expect((await store.conversations.listChats({ ids: [] })).length).toBe(
          0,
        );
      } finally {
        close?.();
      }
    });

    it("listMessages pages BACKWARDS with beforeOrderKey, and rejects both cursors at once", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const messages = await appendChain(store, chat.id, 5);
        const keys = messages.map((m) => m.orderKey);

        // The page immediately above the cursor: the LAST `limit` messages
        // strictly before it, still ascending.
        const page = await store.conversations.listMessages(chat.id, {
          beforeOrderKey: keys[4] as number,
          limit: 2,
        });
        expect(page.map((m) => m.id)).toEqual([
          messages[2]?.id as string,
          messages[3]?.id as string,
        ]);

        // Walking upwards from that page's first message reaches the root.
        const above = await store.conversations.listMessages(chat.id, {
          beforeOrderKey: page[0]?.orderKey as number,
          limit: 2,
        });
        expect(above.map((m) => m.id)).toEqual([
          messages[0]?.id as string,
          messages[1]?.id as string,
        ]);

        // Strictly before: the cursor's own message never comes back.
        const strict = await store.conversations.listMessages(chat.id, {
          beforeOrderKey: keys[0] as number,
        });
        expect(strict.length).toBe(0);

        // Both cursors is a range read with no defined answer to "which end
        // does limit count from", so it is refused rather than guessed.
        await expectRejectsWithCode(
          store.conversations.listMessages(chat.id, {
            afterOrderKey: keys[0] as number,
            beforeOrderKey: keys[4] as number,
          }),
          "invalid_cursor",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("deleteChat removes the chat and EVERY message, off-path branches included", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const survivor = await store.conversations.createChat({});
        const root = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "root",
        });
        const first = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "first answer",
        });
        // A second answer to the same question: `first` is now off-path, and
        // an off-path message is as much a part of this chat as a live one.
        await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "second answer",
          parentMessageId: root.id,
        });
        const survivorMessage = await store.conversations.appendMessage({
          chatId: survivor.id,
          role: "user",
          content: "untouched",
        });

        await store.conversations.deleteChat(chat.id);

        expect(await store.conversations.getChat(chat.id)).toBeNull();
        expect((await store.conversations.listMessages(chat.id)).length).toBe(
          0,
        );
        // The abandoned branch is gone too — proved through a lookup that does
        // not go via the chat, so a store that only unlinked it would fail.
        await expectRejectsWithCode(
          store.conversations.listSiblings(first.id),
          "not_found",
          expect,
        );
        // A neighbouring chat is untouched: the delete was scoped, not a purge.
        expect((await store.conversations.getChat(survivor.id))?.id).toBe(
          survivor.id,
        );
        expect(
          (await store.conversations.listMessages(survivor.id)).map(
            (m) => m.id,
          ),
        ).toEqual([survivorMessage.id]);
      } finally {
        close?.();
      }
    });

    it("a fork of an archived chat is NOT archived", async () => {
      const { store, close } = await create();
      try {
        const source = await store.conversations.createChat({
          title: "Filed away",
        });
        const message = await store.conversations.appendMessage({
          chatId: source.id,
          role: "user",
          content: "still worth forking",
        });
        await store.conversations.updateChat(source.id, { archived: true });

        const fork = await store.conversations.forkChat(source.id, message.id);
        // Starting a conversation that is already filed away is not a state a
        // user can have meant — and the fork would be invisible in the listing
        // it was just created for.
        expect(fork.chat.archived).toBe(false);
        expect(
          (await store.conversations.listChats()).map((c) => c.id),
        ).toEqual([fork.chat.id]);
        // The source keeps its own flag: a fork is a copy, not a move.
        expect((await store.conversations.getChat(source.id))?.archived).toBe(
          true,
        );
      } finally {
        close?.();
      }
    });

    it("deleteChat rejects an unknown chat instead of succeeding silently", async () => {
      const { store, close } = await create();
      try {
        await expectRejectsWithCode(
          store.conversations.deleteChat("chat-nope"),
          "not_found",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("deleteByScope removes a scope's tasks with their attempts, leases and events, and leaves other scopes alone", async () => {
      const { store, close } = await create();
      try {
        const doomed = "chat-doomed";
        const kept = "chat-kept";
        await store.tasks.createTask(taskInScope(doomed, "task-a"));
        await store.tasks.createTask(taskInScope(doomed, "task-b"));
        await store.tasks.createTask(taskInScope(kept, "task-c"));

        // Give one task an attempt, a live lease and an event log — the three
        // things a delete could orphan.
        const attempt = await store.tasks.createAttempt({
          attemptId: "att-a",
          taskId: "task-a",
          ownerId: "worker-1",
        });
        const lease = await store.tasks.acquireLease({
          taskId: "task-a",
          attemptId: attempt.attemptId,
          ownerId: "worker-1",
          ttlMs: 60_000,
        });
        const stamp = createTestEventStamper();
        await store.tasks.appendEvents(
          "task-a",
          [
            stamp({
              type: "run.started",
              runId: "task-a",
              timestamp: new Date().toISOString(),
              data: { model: "m", toolCount: 0 },
            }),
          ],
          { leaseToken: lease.leaseToken },
        );

        // The read the caller checks before deleting: everything in the scope,
        // whatever its status.
        const inScope = await store.tasks.listByScope(doomed);
        expect([...inScope.map((t) => t.taskId)].sort()).toEqual([
          "task-a",
          "task-b",
        ]);

        expect(await store.tasks.deleteByScope(doomed)).toBe(2);
        expect((await store.tasks.listByScope(doomed)).length).toBe(0);
        expect(await store.tasks.getTask("task-a")).toBeNull();
        expect(await store.tasks.getTask("task-b")).toBeNull();
        // The event log went with the task it belonged to: a numbering that
        // survived would make the next task of the same id start mid-stream.
        expect((await store.tasks.listEvents("task-a")).length).toBe(0);
        expect(await store.tasks.nextSeq("task-a")).toBe(0);
        // The lease went too — the token no longer names anything, so a zombie
        // writer holding it is refused rather than served.
        await expectRejectsWithCode(
          store.tasks.renewLease(lease.leaseToken, 60_000),
          "lease_lost",
          expect,
        );
        // A different scope is untouched, and still claimable — so the lease
        // and attempt rows of the deleted scope did not take it down with them.
        expect(
          (await store.tasks.listByScope(kept)).map((t) => t.taskId),
        ).toEqual(["task-c"]);
        const next = await store.tasks.claimNext({
          ownerId: "worker-2",
          now: new Date(),
          scopesBusy: [],
        });
        expect(next?.task.taskId).toBe("task-c");
      } finally {
        close?.();
      }
    });

    // The busy guard belongs to the STORE, not to `ConversationService`. The
    // service checks first, but its check runs before an `await` inside an
    // async transaction — and a concurrent store call FLATTENS into that
    // transaction rather than opening its own, so a task can go
    // `queued → running` in the gap. Only a check the adapter makes in the same
    // synchronous unit as the deletes can be the guarantee, which is why it is
    // graded here, against every adapter. See `TaskStore.deleteByScope`.
    for (const busyStatus of ["running", "waiting_approval"] as TaskStatus[]) {
      it(`deleteByScope REFUSES with chat_busy while a task in the scope is ${busyStatus}, and deletes NOTHING`, async () => {
        const { store, close } = await create();
        try {
          const doomed = "chat-busy";
          await store.tasks.createTask(taskInScope(doomed, "task-live"));
          // A perfectly deletable task beside it: the refusal is all-or-nothing,
          // not "everything except the live one".
          await store.tasks.createTask(taskInScope(doomed, "task-queued"));
          await store.tasks.createTask(taskInScope("chat-kept", "task-c"));

          await store.tasks.transitionTask("task-live", ["queued"], "running", {
            startedAt: new Date().toISOString(),
          });
          if (busyStatus === "waiting_approval") {
            await store.tasks.transitionTask(
              "task-live",
              ["running"],
              "waiting_approval",
            );
          }

          await expectRejectsWithCode(
            store.tasks.deleteByScope(doomed),
            "chat_busy",
            expect,
          );

          // Nothing left the store: not the live task, and not its queued
          // neighbour either.
          expect(
            [
              ...(await store.tasks.listByScope(doomed)).map((t) => t.taskId),
            ].sort(),
          ).toEqual(["task-live", "task-queued"]);
          expect((await store.tasks.getTask("task-live"))?.status).toBe(
            busyStatus,
          );

          // The refusal NAMES what is holding it, so a UI can point at the run
          // to cancel or wait for instead of saying "busy, try again".
          let details: { taskIds?: unknown; statuses?: unknown } | undefined;
          try {
            await store.tasks.deleteByScope(doomed);
          } catch (err) {
            details = (
              err as { details?: { taskIds?: unknown; statuses?: unknown } }
            ).details;
          }
          expect(details?.taskIds).toEqual(["task-live"]);
          expect(details?.statuses).toEqual([busyStatus]);

          // And the scope is deletable again the moment the work ends — the
          // refusal is a "not yet", not a permanent one.
          await store.tasks.transitionTask(
            "task-live",
            [busyStatus],
            "completed",
            { finishedAt: new Date().toISOString() },
          );
          expect(await store.tasks.deleteByScope(doomed)).toBe(2);
          expect((await store.tasks.getTask("task-c"))?.taskId).toBe("task-c");
        } finally {
          close?.();
        }
      });
    }

    it("deleteByScope refuses a task a CONCURRENT claim took mid-delete, so nothing is ever claimed AND deleted", async () => {
      const { store, close } = await create();
      try {
        const doomed = "chat-raced";
        await store.tasks.createTask(taskInScope(doomed, "task-raced"));

        // A holder, because the assignment happens inside a callback and a
        // plain `let` would be narrowed to `null` at every use site below.
        const claimed: { value: ClaimedTask | null } = { value: null };
        let deleted: number | undefined;
        let refusal: { code?: string } | undefined;
        try {
          deleted = await store.transaction(async (tx) => {
            // The caller's fast-path check, exactly as
            // `ConversationService.deleteChat` makes it: nothing live, so the
            // delete may proceed.
            expect(
              (await tx.tasks.listByScope(doomed)).map((t) => t.status),
            ).toEqual(["queued"]);
            // THE INTERLEAVING, forced rather than hoped for: a worker claims
            // the task between that check and the delete below. The call is
            // made on the STORE, not on `tx`, and it still lands inside this
            // transaction — flattening is what makes the check above unable to
            // be the guarantee.
            claimed.value = await store.tasks.claimNext({
              ownerId: "worker-1",
              now: new Date(),
              scopesBusy: [],
            });
            return await tx.tasks.deleteByScope(doomed);
          });
        } catch (err) {
          refusal = err as { code?: string };
        }

        // The race really was driven — the claim is awaited to completion
        // before the delete is even called, so it cannot have lost.
        expect(claimed.value?.task.taskId).toBe("task-raced");

        // THE HEADLINE: claimed AND deleted is the state that must not exist.
        // A worker holding a lease on a row that is gone will write its events,
        // its attempt end and its transition into a task nothing can name.
        expect((await store.tasks.getTask("task-raced")) !== null).toBe(true);
        expect(refusal?.code).toBe("chat_busy");
        expect(deleted).toBe(undefined);
      } finally {
        close?.();
      }
    });

    it("deleteByChat removes a chat's proposals and their outcomes, by chat and not by scope", async () => {
      const { store, close } = await create();
      try {
        const at = new Date().toISOString();
        const base = {
          scopeKey: "doc-1",
          toolName: "test.tool",
          kind: "test.kind",
          risk: "low" as const,
          envelope: {},
          operations: [],
          warnings: [],
          truncated: false,
          createdAt: at,
        };
        await store.proposals.create({
          ...base,
          id: "prp-1",
          chatId: "chat-1",
        });
        await store.proposals.create({
          ...base,
          id: "prp-2",
          chatId: "chat-1",
        });
        // SAME SCOPE, different chat — the record a scope-keyed delete would
        // have taken by mistake.
        await store.proposals.create({
          ...base,
          id: "prp-3",
          chatId: "chat-2",
        });

        // Give one proposal a real outcome to strand.
        await store.proposals.transition("prp-1", ["pending"], "approved");
        await store.proposals.transition("prp-1", ["approved"], "applying", {
          operationId: "op-1",
        });
        await store.proposals.recordOutcome("op-1", {
          status: "applied",
          appliedOps: 1,
          failedOps: [],
        });

        expect(await store.proposals.deleteByChat("chat-1")).toBe(2);
        expect(await store.proposals.get("prp-1")).toBeNull();
        expect(await store.proposals.get("prp-2")).toBeNull();
        expect(await store.proposals.getOutcome("op-1")).toBeNull();
        // The other chat's proposal survived, scope-sharing and all.
        expect((await store.proposals.get("prp-3"))?.chatId).toBe("chat-2");
        expect((await store.proposals.listByChat("chat-1")).length).toBe(0);
      } finally {
        close?.();
      }
    });
  });
}
