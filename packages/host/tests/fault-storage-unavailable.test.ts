/**
 * ONEMIND-349 · M0 fault matrix §A row 13 — STORAGE UNAVAILABLE for the
 * durable store.
 *
 * The matrix row demands an executable check that when the storage layer
 * cannot be opened, or is lost mid-session, the host SURFACES the outage (the
 * driver sees a storage-class failure, not a silence), NOTHING is silently
 * lost (committed state survives the outage and is read back unchanged), and
 * there is a RECOVERY path once storage returns (the interrupted turn is
 * re-driven to exactly one committed answer). Three edges, matching the
 * row's three promises:
 *
 * 1. Store cannot be opened (real permission fault on the real sqlite file):
 *    reopening the store after a completed turn throws; after the device is
 *    restored the same file opens and every committed record reads back
 *    byte-identical — the outage lost nothing silently.
 * 2. Storage dies mid-session (whole-write loss injected at the port seam,
 *    the `claim-failure-injection.test.ts` rig pattern): the driver's
 *    `executeTask` rejects with the storage error, the task is NOT landed
 *    completed, no `run.completed` is committed, and the bookkeeping failures
 *    the runner swallows are LOUD in the logger. After the device returns the
 *    task is parked `interrupted`, requeued and re-driven to exactly one
 *    committed answer with a gap-free event sequence.
 * 3. Submit against unavailable storage refuses explicitly and persists
 *    nothing: no user message, no task row, no assistant placeholder — and
 *    the retry after restore produces the turn normally.
 */
import { describe, expect, it } from "bun:test";
import { chmodSync } from "node:fs";
import type { AiRunEvent } from "@agentkit/contracts";
import { SqliteAssistantStore } from "../../adapters-sqlite/src/index.js";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import type { AssistantStore } from "../src/index.js";
import {
  StorageFaultError,
  createFaultScratch,
  createStorageFaultRig,
} from "./storage-fault-helpers.js";
import { execute, fixture } from "./provider-continuation-helpers.js";

/** Records what the runner logged so "swallowed" can be shown to be LOUD. */
function recordingLogger() {
  const lines: { level: string; message: string; meta?: unknown }[] = [];
  return {
    lines,
    logger: {
      debug: (message: string, meta?: unknown) =>
        lines.push({ level: "debug", message, meta }),
      info: (message: string, meta?: unknown) =>
        lines.push({ level: "info", message, meta }),
      warn: (message: string, meta?: unknown) =>
        lines.push({ level: "warn", message, meta }),
      error: (message: string, meta?: unknown) =>
        lines.push({ level: "error", message, meta }),
    },
  };
}

async function terminalEvents(store: AssistantStore, runId: string) {
  const events = (await store.tasks.listEvents(runId)) as AiRunEvent[];
  return events.filter(
    (event) =>
      event.type === "run.completed" ||
      event.type === "run.failed" ||
      event.type === "run.cancelled",
  );
}

describe("storage unavailable (M0 fault matrix row 13)", () => {
  it("a store that cannot be opened fails loudly, and the restored file reads back unchanged", async () => {
    const scratch = createFaultScratch("open");
    try {
      // Commit a real turn on the real adapter, then close the handle.
      const store = new SqliteAssistantStore(scratch.path);
      const env = await fixture({ store });
      const submitted = await env.runner.submitMessage({
        chatId: env.chatId,
        content: "persist me",
      });
      await execute(env.runner, store, submitted.runId);
      const beforeMessages = await store.conversations.listMessages(env.chatId);
      const beforeTask = await store.tasks.getTask(submitted.runId);
      const beforeEvents = await store.tasks.listEvents(submitted.runId);
      expect(beforeTask?.status).toBe("completed");
      store.close();

      // Permission fault: the file exists but the process cannot open it.
      chmodSync(scratch.path, 0o000);
      let openError: unknown;
      try {
        new SqliteAssistantStore(scratch.path);
      } catch (err) {
        openError = err;
      }
      expect(openError).toBeInstanceOf(Error);
      expect(String((openError as Error).message)).toMatch(
        /unable to open database file|permission denied/i,
      );

      // Restore the device: the SAME file opens and reports exactly what was
      // committed before the outage — byte-identical, not reconstructed.
      chmodSync(scratch.path, 0o600);
      const reopened = new SqliteAssistantStore(scratch.path);
      expect(
        JSON.stringify(await reopened.conversations.listMessages(env.chatId)),
      ).toBe(JSON.stringify(beforeMessages));
      expect(
        JSON.stringify(await reopened.tasks.getTask(submitted.runId)),
      ).toBe(JSON.stringify(beforeTask));
      expect(
        JSON.stringify(await reopened.tasks.listEvents(submitted.runId)),
      ).toBe(JSON.stringify(beforeEvents));
      reopened.close();
    } finally {
      scratch.cleanup();
    }
  });

  for (const adapter of ["sqlite", "memory"] as const) {
    it(`a mid-session storage loss surfaces to the driver, commits no false success, and recovers (${adapter})`, async () => {
      const scratch = adapter === "sqlite" ? createFaultScratch("mid") : null;
      try {
        const base =
          adapter === "sqlite"
            ? new SqliteAssistantStore(scratch!.path)
            : new MemoryAssistantStore({});
        const rig = createStorageFaultRig(base);
        const log = recordingLogger();
        const env = await fixture({
          store: rig.faulty,
          overrides: { logger: log.logger },
        });
        const submitted = await env.runner.submitMessage({
          chatId: env.chatId,
          content: "Find it",
        });

        // Claim while storage is alive, then pull the device mid-session:
        // EVERY durable write from here on fails with an EIO-class error.
        const task = await base.tasks.getTask(submitted.runId);
        const claim = await base.tasks.claimNext({
          ownerId: "test",
          now: new Date(task?.availableAt ?? Date.now()),
          scopesBusy: [],
        });
        if (!claim || claim.task.taskId !== submitted.runId)
          throw new Error("Unexpected task claim");
        rig.armPattern(/^(tasks|conversations)\./, {
          code: "EIO",
          message: "disk I/O error (database file removed mid-session)",
        });

        let driverError: unknown;
        try {
          await env.runner.executeTask({
            task: claim.task,
            attemptId: claim.attempt.attemptId,
            leaseToken: claim.lease.leaseToken,
            signal: new AbortController().signal,
          });
        } catch (err) {
          driverError = err;
        }

        // Surfaced, not swallowed: the driver sees the storage-class failure.
        expect(driverError).toBeInstanceOf(StorageFaultError);
        expect((driverError as StorageFaultError).code).toBe("EIO");
        // No false success: nothing terminal was committed, and the task was
        // not landed — the bookkeeping writes died with the device, and the
        // runner logged every swallowed one instead of hushing it.
        expect(await base.tasks.listEvents(submitted.runId)).not.toContainEqual(
          expect.objectContaining({ type: "run.completed" }),
        );
        expect((await base.tasks.getTask(submitted.runId))?.status).toBe(
          "running",
        );
        expect(
          log.lines.filter(
            (line) =>
              line.level === "warn" &&
              /could not (record|finalize)/.test(line.message),
          ).length,
        ).toBeGreaterThan(0);
        // No silent loss: the user message submitted BEFORE the outage is
        // still in the durable store.
        const messages = await base.conversations.listMessages(env.chatId);
        expect(
          messages.some(
            (message) =>
              message.role === "user" &&
              JSON.stringify(message.content).includes("Find it"),
          ),
        ).toBe(true);

        // Recovery: the device is back. The stranded attempt is parked the
        // way a dead worker's is (interrupted), requeued, and re-driven to
        // exactly one committed answer under a fresh lease.
        rig.clear();
        await base.tasks.transitionTask(
          submitted.runId,
          ["running"],
          "interrupted",
          { error: "storage unavailable mid-session" },
        );
        await base.tasks.transitionTask(
          submitted.runId,
          ["interrupted"],
          "queued",
        );
        await execute(env.runner, rig.faulty, submitted.runId);

        expect((await base.tasks.getTask(submitted.runId))?.status).toBe(
          "completed",
        );
        const terminals = await terminalEvents(base, submitted.runId);
        expect(
          terminals.filter((event) => event.type === "run.completed"),
        ).toHaveLength(1);
        const events = (await base.tasks.listEvents(submitted.runId)) as {
          seq: number;
        }[];
        expect(events.map((event) => event.seq)).toEqual(
          events.map((_, index) => index + events[0]!.seq),
        );
        const final = await base.conversations.getMessage(
          submitted.assistantMessageId,
        );
        expect(final?.metadata["placeholder"]).toBe(false);
      } finally {
        scratch?.cleanup();
      }
    });
  }

  it("submitting against unavailable storage refuses explicitly and persists nothing", async () => {
    const base = new MemoryAssistantStore({});
    const rig = createStorageFaultRig(base);
    const env = await fixture({ store: rig.faulty });
    const messagesBefore = await base.conversations.listMessages(env.chatId);

    rig.armPattern(/^(tasks|conversations)\./, {
      code: "SQLITE_CANTOPEN",
      message: "unable to open database file",
    });
    let submitError: unknown;
    try {
      await env.runner.submitMessage({
        chatId: env.chatId,
        content: "never admitted",
      });
    } catch (err) {
      submitError = err;
    }
    expect(submitError).toBeInstanceOf(StorageFaultError);
    expect((submitError as StorageFaultError).code).toBe("SQLITE_CANTOPEN");

    // Nothing persisted — the chat holds exactly what it held before, and no
    // half turn exists to be mistaken for an admitted message.
    expect(
      JSON.stringify(await base.conversations.listMessages(env.chatId)),
    ).toBe(JSON.stringify(messagesBefore));

    // Retry path: the device is back and the same submit produces the turn.
    rig.clear();
    const retried = await env.runner.submitMessage({
      chatId: env.chatId,
      content: "never admitted",
    });
    await execute(env.runner, rig.faulty, retried.runId);
    expect((await base.tasks.getTask(retried.runId))?.status).toBe("completed");
  });
});
