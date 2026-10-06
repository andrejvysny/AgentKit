import { describe, expect, it } from "bun:test";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SingleProcessTaskRunner } from "../src/index.js";
import { waitFor } from "./support/task-runner-harness.js";

function clock() {
  let time = Date.now();
  return {
    now: () => new Date(time),
    nowIso: () => new Date(time).toISOString(),
    advance: (ms: number) => {
      time += ms;
    },
  };
}

describe("manual recovery", () => {
  it("parks queued and expired work across restart; resumes only by explicit identity", async () => {
    const time = clock();
    const store = new MemoryAssistantStore({ clock: time });
    for (const id of ["queued", "stale"])
      await store.tasks.createTask({
        taskId: id,
        scopeId: id,
        kind: "work",
        payload: {},
      });
    const claimed = await store.tasks.claimNext({
      ownerId: "dead",
      now: time.now(),
      scopesBusy: [],
      kinds: ["work"],
    });
    expect(claimed).not.toBeNull();
    time.advance(60_000);
    const runner = new SingleProcessTaskRunner({
      store,
      clock: time,
      recoveryMode: "manual",
      pollMs: 1,
    });
    let calls = 0;
    await expect(
      runner.startWorker({
        execute: async () => {
          calls++;
        },
      }),
    ).rejects.toMatchObject({ code: "recovery_required" });
    await runner.recover();
    expect((await store.tasks.getTask("queued"))?.status).toBe("interrupted");
    expect((await store.tasks.getTask("stale"))?.status).toBe("interrupted");
    const reboot = new SingleProcessTaskRunner({
      store,
      clock: time,
      recoveryMode: "manual",
      pollMs: 1,
    });
    await reboot.recover();
    const handle = await reboot.startWorker({
      execute: async () => {
        calls++;
      },
    });
    try {
      expect(calls).toBe(0);
      await reboot.resume("queued");
      await waitFor(
        async () =>
          (await store.tasks.getTask("queued"))?.status === "completed",
        "explicit resume",
      );
      expect(calls).toBe(1);
      expect((await store.tasks.getTask("stale"))?.status).toBe("interrupted");
      await expect(reboot.resume("queued")).rejects.toMatchObject({
        code: "invalid_task_transition",
      });
      await reboot.requestCancel("stale");
      expect((await store.tasks.getTask("stale"))?.status).toBe("cancelled");
    } finally {
      await handle.stop();
    }
  });
});

it("shutdown cancellation aborts active work before claiming another task", async () => {
  const store = new MemoryAssistantStore();
  const runner = new SingleProcessTaskRunner({
    store,
    shutdownMode: "cancel",
    pollMs: 1,
  });
  for (const id of ["active", "next"])
    await store.tasks.createTask({
      taskId: id,
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
  const started: string[] = [];
  let aborted = false;
  const handle = await runner.startWorker(
    {
      execute: async ({ taskId, signal }) => {
        started.push(taskId);
        await new Promise<void>((resolve) =>
          signal.addEventListener(
            "abort",
            () => {
              aborted = true;
              resolve();
            },
            { once: true },
          ),
        );
      },
    },
    { concurrency: 1 },
  );
  await waitFor(() => started.length === 1, "worker start");
  await handle.stop();
  expect(aborted).toBe(true);
  expect(started).toEqual(["active"]);
  expect((await store.tasks.getTask("active"))?.status).toBe("cancelled");
  expect((await store.tasks.getTask("next"))?.status).toBe("queued");
});
