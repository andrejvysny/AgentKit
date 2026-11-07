/**
 * AGENTKIT-73, facade leg: the static fast path — a chat the host serves
 * with no contributors and no guards — has no session, so the session
 * surface can never close its uncertain receipts (`session_not_found` every
 * time; the permanent dead end). The chat-keyed resolve route is the exit:
 * the same ownership, result and audit rules, keyed by the receipt's chat,
 * and it still never replays the effect.
 */
import { expect, test } from "bun:test";
import { NativeStore } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";
import { testProfile } from "./helpers/profiles.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

const HEX64 = "0123456789abcdef".repeat(4);

nativeTest(
  "the static fast path resolves sessionless receipts by chat, never by session",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService({
      profiles: [testProfile("p1", { permissionMode: "read_only" })],
    });
    try {
      // Seed exactly what a crashed static fast-path turn leaves behind: a
      // chat with no session, a settled run, and an admitted receipt that
      // never received its result.
      await store.callStore("conversations.createChat", {
        input: { id: "c-static" },
      });
      await store.callStore("tasks.createTask", {
        input: {
          taskId: "run-static",
          // Not `chat.turn`: the service runner must never claim this seed,
          // exactly as the parity fixtures seed with a neutral kind.
          kind: "fixture",
          scopeId: "c-static",
          payload: { chatId: "c-static" },
        },
      });
      const lease = await store.callStore<{ leaseToken: string }>(
        "tasks.acquireLease",
        {
          input: {
            taskId: "run-static",
            attemptId: "a1",
            ownerId: "owner",
            ttlMs: 60_000,
          },
        },
      );
      expect(
        await store.callStore<boolean>("toolExecutions.admit", {
          input: {
            executionId: "exec-static",
            runId: "run-static",
            chatId: "c-static",
            toolCallId: "call-1",
            toolName: "write_file",
            argsSha256: HEX64,
            contextSha256: HEX64,
            attemptId: "a1",
          },
          fence: { taskId: "run-static", leaseToken: lease.leaseToken },
        } as never),
      ).toBe(true);
      await store.callStore("tasks.transitionTask", {
        taskId: "run-static",
        from: ["queued"],
        to: "running",
      });
      await store.callStore("tasks.transitionTask", {
        taskId: "run-static",
        from: ["running"],
        to: "failed",
      });

      // The session surface is the dead end: no session owns this chat, so
      // it can never close the receipt.
      await expect(
        service.resolveUncertainExecution({
          sessionId: "c-static",
          executionId: "exec-static",
          outcome: "not_started",
        }),
      ).rejects.toMatchObject({ code: "session_not_found" });

      // An unknown chat never reveals whether the receipt exists elsewhere.
      await expect(
        service.resolveUncertainExecutionForChat({
          chatId: "c-other",
          executionId: "exec-static",
          outcome: "not_started",
        }),
      ).rejects.toMatchObject({ code: "not_found" });
      // The result rules are the session route's: completed requires one,
      // nothing else may carry one.
      await expect(
        service.resolveUncertainExecutionForChat({
          chatId: "c-static",
          executionId: "exec-static",
          outcome: "completed",
        }),
      ).rejects.toMatchObject({ code: "invalid_input" });

      // The chat route closes it — audited, and nothing was replayed.
      const record = await service.resolveUncertainExecutionForChat({
        chatId: "c-static",
        executionId: "exec-static",
        outcome: "completed",
        result: "the file is on disk",
        note: "verified by hand",
        resolvedBy: "operator",
      });
      expect(record.status).toBe("completed");
      expect(record.resolution).toMatchObject({
        outcome: "completed",
        note: "verified by hand",
        resolvedBy: "operator",
      });
      expect(typeof record.resolution?.resolvedAt).toBe("number");
      // Any further close of a finished receipt refuses.
      await expect(
        service.resolveUncertainExecutionForChat({
          chatId: "c-static",
          executionId: "exec-static",
          outcome: "not_started",
        }),
      ).rejects.toMatchObject({ code: "receipt_not_uncertain" });

      await service.close();
      await expect(
        service.resolveUncertainExecutionForChat({
          chatId: "c-static",
          executionId: "exec-static",
          outcome: "not_started",
        }),
      ).rejects.toMatchObject({ code: "closed" });
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);
