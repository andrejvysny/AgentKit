/**
 * ONEMIND-349 · M0 fault matrix §A row 14 — STORAGE FULL.
 *
 * The row's promise: when the write path returns `ENOSPC` mid-write, the
 * durable-state invariants still hold (a turn is never PARTIALLY committed —
 * the user message, the task row and the placeholder live and die together,
 * and a turn that never completed never LOOKS completed), the failure is
 * USER-VISIBLE (the driver's call rejects with the storage error, not a
 * silently truncated stream), and there is a RETRY path once space frees.
 *
 * The rig is the claim-failure-injection pattern moved one layer up: named
 * port methods, faulted from the nth call on, on BOTH reference adapters
 * (sqlite — the OneMind.db shape — and memory, the conformance mirror), so
 * "committed together" is graded against a real `BEGIN IMMEDIATE` /
 * `ROLLBACK` and against the adapter that promises to mirror it.
 */
import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { SqliteAssistantStore } from "../../adapters-sqlite/src/index.js";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import type { AssistantStore } from "../src/index.js";
import {
  StorageFaultError,
  TRANSACTION_KEY,
  createFaultScratch,
  createStorageFaultRig,
} from "./storage-fault-helpers.js";
import { execute, fixture } from "./provider-continuation-helpers.js";

const ENOSPC = { code: "ENOSPC" } as const;

async function newEnvironment(
  adapter: "sqlite" | "memory",
  scratchPath?: string,
) {
  const base =
    adapter === "sqlite"
      ? new SqliteAssistantStore(scratchPath!)
      : new MemoryAssistantStore({});
  const rig = createStorageFaultRig(base);
  const env = await fixture({ store: rig.faulty });
  return { base, rig, env };
}

/** Read a durable read-only view through the untouched adapter. */
async function durableSnapshot(store: AssistantStore, chatId: string) {
  return {
    messages: await store.conversations.listMessages(chatId),
    tasks: await store.tasks.listByScope(chatId),
  };
}

describe("storage full (M0 fault matrix row 14)", () => {
  for (const adapter of ["sqlite", "memory"] as const) {
    it(`ENOSPC inside the submit transaction commits nothing at all (${adapter})`, async () => {
      const scratch =
        adapter === "sqlite" ? createFaultScratch("submit") : null;
      try {
        const { base, rig, env } = await newEnvironment(adapter, scratch?.path);
        const before = await durableSnapshot(base, env.chatId);
        const providerCallsBefore = env.sent.length;

        // The device is full exactly when the turn's task row goes in — the
        // user message and placeholder that follow in the SAME transaction
        // must never exist without it.
        rig.arm("tasks.createTask", ENOSPC);
        let submitError: unknown;
        try {
          await env.runner.submitMessage({
            chatId: env.chatId,
            content: "turn the disk cannot hold",
          });
        } catch (err) {
          submitError = err;
        }

        // User-visible failure carrying the storage code.
        expect(submitError).toBeInstanceOf(StorageFaultError);
        expect((submitError as StorageFaultError).code).toBe("ENOSPC");
        // No partially committed turn: the chat is byte-identical to before.
        const after = await durableSnapshot(base, env.chatId);
        expect(JSON.stringify(after.messages)).toBe(
          JSON.stringify(before.messages),
        );
        expect(env.sent).toHaveLength(providerCallsBefore);

        // Retry path: space freed, the same submit produces the full turn.
        rig.clear();
        const retried = await env.runner.submitMessage({
          chatId: env.chatId,
          content: "turn the disk cannot hold",
        });
        await execute(env.runner, rig.faulty, retried.runId);
        expect((await base.tasks.getTask(retried.runId))?.status).toBe(
          "completed",
        );
        const finalMessages = await base.conversations.listMessages(env.chatId);
        expect(
          finalMessages.filter((message) => message.role === "user"),
        ).toHaveLength(1);
      } finally {
        scratch?.cleanup();
      }
    });
  }

  for (const adapter of ["sqlite", "memory"] as const) {
    it(`ENOSPC on the final durable write leaves no half-committed turn and retries clean (${adapter})`, async () => {
      const probeScratch =
        adapter === "sqlite" ? createFaultScratch("probe") : null;
      const scratch =
        adapter === "sqlite" ? createFaultScratch("commit") : null;
      try {
        // Probe run: identical inputs, no fault — this tells us WHICH calls
        // are the commit-point writes, the way the claim suite enumerates its
        // injection points from the clean path.
        const probe = await newEnvironment(adapter, probeScratch?.path);
        const probeRun = await probe.env.runner.submitMessage({
          chatId: probe.env.chatId,
          content: "Find it",
        });
        await execute(probe.env.runner, probe.rig.faulty, probeRun.runId);
        const totalUpdates = probe.rig.calls("conversations.updateMessage");
        const totalAppends = probe.rig.calls("tasks.appendEvents");
        expect(totalUpdates).toBeGreaterThan(0);
        // The clean run's durable writes are fixed: the penultimate
        // `appendEvents` carries `run.completed` (the commit point of the
        // turn), the last one belongs to the finalization that follows it.
        // Faulting the commit-point append must commit NOTHING terminal.
        expect(totalAppends).toBeGreaterThan(2);

        // Fault run: the terminal event commit fails with ENOSPC.
        const { base, rig, env } = await newEnvironment(adapter, scratch?.path);
        const run = await env.runner.submitMessage({
          chatId: env.chatId,
          content: "Find it",
        });
        rig.arm("tasks.appendEvents", { ...ENOSPC, after: totalAppends - 2 });
        let driverError: unknown;
        try {
          await execute(env.runner, rig.faulty, run.runId);
        } catch (err) {
          driverError = err;
        }

        // User-visible ENOSPC; no fabricated success.
        expect(driverError).toBeInstanceOf(StorageFaultError);
        expect((driverError as StorageFaultError).code).toBe("ENOSPC");
        const events = (await base.tasks.listEvents(run.runId)) as AiRunEvent[];
        expect(events.some((event) => event.type === "run.completed")).toBe(
          false,
        );
        // The durable turn is either fully committed or absent: the assistant
        // message is still a placeholder (or its commit rolled back whole),
        // never a finalized answer without a terminal event.
        const placeholder = await base.conversations.getMessage(
          run.assistantMessageId,
        );
        if (placeholder) expect(placeholder.metadata["placeholder"]).toBe(true);
        // The event log is a gap-free prefix — a truncated stream, never a
        // hole-riddled one.
        const seqs = events.map((event) => event.seq);
        expect(seqs).toEqual(
          seqs.map((_first, index) => (seqs[0] as number) + index),
        );

        // Retry path: free the device, park the stranded attempt the way a
        // dead worker's is parked, requeue, re-drive — exactly one completion.
        rig.clear();
        await base.tasks.transitionTask(run.runId, ["running"], "interrupted", {
          error: "storage full mid-commit",
        });
        await base.tasks.transitionTask(run.runId, ["interrupted"], "queued");
        await execute(env.runner, rig.faulty, run.runId);
        expect((await base.tasks.getTask(run.runId))?.status).toBe("completed");
        const after = (await base.tasks.listEvents(run.runId)) as AiRunEvent[];
        expect(
          after.filter((event) => event.type === "run.completed"),
        ).toHaveLength(1);
        const finalMessage = await base.conversations.getMessage(
          run.assistantMessageId,
        );
        expect(finalMessage?.metadata["placeholder"]).toBe(false);
      } finally {
        probeScratch?.cleanup();
        scratch?.cleanup();
      }
    });
  }

  it("ENOSPC that even refuses BEGIN commits nothing and recovers", async () => {
    const { base, rig, env } = await newEnvironment("memory");
    const before = await base.conversations.listMessages(env.chatId);
    rig.arm(TRANSACTION_KEY, ENOSPC);
    let submitError: unknown;
    try {
      await env.runner.submitMessage({
        chatId: env.chatId,
        content: "no room for a journal",
      });
    } catch (err) {
      submitError = err;
    }
    expect(submitError).toBeInstanceOf(StorageFaultError);
    expect((submitError as StorageFaultError).code).toBe("ENOSPC");
    expect(
      JSON.stringify(await base.conversations.listMessages(env.chatId)),
    ).toBe(JSON.stringify(before));
    rig.clear();
    const retried = await env.runner.submitMessage({
      chatId: env.chatId,
      content: "no room for a journal",
    });
    await execute(env.runner, rig.faulty, retried.runId);
    expect((await base.tasks.getTask(retried.runId))?.status).toBe("completed");
  });
});
