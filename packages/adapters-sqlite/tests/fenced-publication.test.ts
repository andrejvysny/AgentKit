import { describe, expect, it } from "bun:test";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import { createEventStamper } from "../../core/src/index.js";
import {
  createRunProjector,
  LeaseLostError,
  type AssistantStore,
  type Clock,
} from "@agentkit/host";
import { SqliteAssistantStore } from "../src/index.js";

for (const adapter of ["memory", "sqlite"] as const) {
  describe(`${adapter} fenced publication`, () => {
    async function fixture() {
      let now = Date.now();
      const clock: Clock = {
        now: () => new Date(now),
        nowIso: () => new Date(now).toISOString(),
      };
      const store: AssistantStore =
        adapter === "memory"
          ? new MemoryAssistantStore({ clock })
          : new SqliteAssistantStore(":memory:", { clock });
      const chat = await store.conversations.createChat({});
      await store.tasks.createTask({
        taskId: "run",
        kind: "chat.turn",
        scopeId: chat.id,
        payload: {},
      });
      const claim = (await store.tasks.claimNext({
        ownerId: "one",
        now: clock.now(),
        scopesBusy: [],
      }))!;
      const message = await store.conversations.appendMessage({
        chatId: chat.id,
        runId: "run",
        role: "assistant",
        content: "",
        metadata: { placeholder: true },
      });
      const ctx = {
        task: claim.task,
        attemptId: claim.attempt.attemptId,
        leaseToken: claim.lease.leaseToken,
      };
      return {
        store,
        clock,
        chat,
        message,
        ctx,
        expire: () => {
          now += 30_001;
        },
      };
    }

    it("refuses expired message and event publication without a recovery sweep", async () => {
      const { store, chat, message, ctx, expire } = await fixture();
      expire();
      await expect(
        store.conversations.updateMessage(
          message.id,
          { content: "zombie" },
          { taskId: "run", leaseToken: ctx.leaseToken },
        ),
      ).rejects.toBeInstanceOf(LeaseLostError);
      await expect(
        store.tasks.appendEvents(
          "run",
          [
            {
              contractVersion: "1",
              eventId: "expired",
              seq: 0,
              type: "host.test",
              timestamp: new Date().toISOString(),
            },
          ],
          { leaseToken: ctx.leaseToken },
        ),
      ).rejects.toBeInstanceOf(LeaseLostError);
      expect(
        (await store.conversations.listMessages(chat.id)).find(
          (record) => record.id === message.id,
        )?.content,
      ).toBe("");
    });

    it("projects a repeated event once and repairs a persisted projection gap", async () => {
      const { store, clock, chat, message, ctx } = await fixture();
      const projector = createRunProjector({ store, clock });
      const input = { chatId: chat.id, assistantMessageId: message.id };
      const state = projector.createState(input);
      const stamp = createEventStamper({
        attemptId: ctx.attemptId,
        firstSeq: 0,
      });
      const event = stamp({
        type: "run.message.completed",
        runId: "run",
        timestamp: clock.nowIso(),
        data: {
          content: "tools",
          toolCallCount: 1,
          toolCalls: [{ id: "call", name: "test", argumentsJson: "{}" }],
        },
      });
      await store.tasks.appendEvents("run", [event], {
        leaseToken: ctx.leaseToken,
      });
      await projector.replay(ctx, state);
      await projector.project(ctx, state, event);
      const replayed = projector.createState(input);
      await projector.replay(ctx, replayed);
      expect(
        (await store.conversations.listMessages(chat.id)).filter(
          (record) => record.metadata.internal,
        ),
      ).toHaveLength(1);
      expect(replayed.toolCallIds.size).toBe(1);
    });

    it("repairs an event whose projection threw, without duplicate internal messages", async () => {
      const { store, clock, chat, message, ctx } = await fixture();
      const projector = createRunProjector({ store, clock });
      const state = projector.createState({
        chatId: chat.id,
        assistantMessageId: message.id,
      });
      const event = createEventStamper()({
        type: "run.message.completed",
        runId: "run",
        timestamp: clock.nowIso(),
        data: {
          content: "read",
          toolCallCount: 1,
          toolCalls: [{ id: "call", name: "read", argumentsJson: "{}" }],
        },
      });
      const transaction = store.transaction.bind(store);
      store.transaction = (fn) =>
        transaction(async (tx) => {
          await fn(tx);
          throw new Error("crash after projection");
        });
      await expect(projector.project(ctx, state, event)).rejects.toThrow(
        "crash after projection",
      );
      store.transaction = transaction;
      expect(await store.tasks.listEvents("run")).toHaveLength(1);
      expect(state.projectedSeq).toBe(-1);
      await projector.replay(ctx, state);
      expect(
        (await store.conversations.listMessages(chat.id)).filter(
          (record) => record.metadata.internal,
        ),
      ).toHaveLength(1);
    });

    it("refuses a superseded owner's reset, projection, and terminal verdict", async () => {
      const { store, clock, chat, message, ctx } = await fixture();
      const projector = createRunProjector({ store, clock });
      const state = projector.createState({
        chatId: chat.id,
        assistantMessageId: message.id,
      });
      const event = createEventStamper()({
        type: "run.message.delta",
        runId: "run",
        timestamp: clock.nowIso(),
        data: { delta: "old" },
      });
      await projector.project(ctx, state, event);
      const attempt = await store.tasks.createAttempt({
        attemptId: "second",
        taskId: "run",
        ownerId: "two",
      });
      const current = await store.tasks.acquireLease({
        taskId: "run",
        attemptId: attempt.attemptId,
        ownerId: "two",
        ttlMs: 30_000,
      });
      await store.conversations.updateMessage(
        message.id,
        { content: "new" },
        { taskId: "run", leaseToken: current.leaseToken },
      );
      await expect(projector.project(ctx, state, event)).rejects.toBeInstanceOf(
        LeaseLostError,
      );
      await expect(
        store.conversations.updateMessage(
          message.id,
          { content: "" },
          { taskId: "run", leaseToken: ctx.leaseToken },
        ),
      ).rejects.toBeInstanceOf(LeaseLostError);
      await expect(
        store.tasks.transitionTask(
          "run",
          ["running"],
          "completed",
          {},
          { leaseToken: ctx.leaseToken },
        ),
      ).rejects.toBeInstanceOf(LeaseLostError);
      expect((await store.conversations.getMessage(message.id))?.content).toBe(
        "new",
      );
      expect((await store.tasks.getTask("run"))?.status).toBe("running");
    });

    it("flushes the crash tail during replay and resets earlier retry text", async () => {
      const { store, clock, chat, message, ctx } = await fixture();
      const stamp = createEventStamper();
      const events = [
        stamp({
          type: "run.message.delta",
          runId: "run",
          timestamp: clock.nowIso(),
          data: { delta: "old" },
        }),
        stamp({
          type: "run.warning",
          runId: "run",
          timestamp: clock.nowIso(),
          data: {
            code: "retry_pass",
            message: "retry",
            pass: 2,
            reason: "chat_only",
          },
        }),
        stamp({
          type: "run.message.delta",
          runId: "run",
          timestamp: clock.nowIso(),
          data: { delta: "new" },
        }),
      ];
      await store.tasks.appendEvents("run", events, {
        leaseToken: ctx.leaseToken,
      });
      const projector = createRunProjector({ store, clock });
      const state = projector.createState({
        chatId: chat.id,
        assistantMessageId: message.id,
      });
      await projector.replay(ctx, state);
      expect(state.content).toBe("new");
      expect((await store.conversations.getMessage(message.id))?.content).toBe(
        "new",
      );
    });
  });
}
