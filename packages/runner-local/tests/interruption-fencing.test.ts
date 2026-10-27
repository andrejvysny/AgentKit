import { expect, it } from "bun:test";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import {
  TaskInterruptedError,
  type AssistantStore,
  type ClaimedTask,
  type TaskStore,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "../src/index.js";
import { waitFor } from "./support/task-runner-harness.js";

for (const adapter of ["memory", "sqlite"] as const) {
  it(`${adapter}: a stale typed interruption cannot park a resumed attempt`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    let replacement: ClaimedTask | null = null;
    let fenced = false;
    const interrupt = store.tasks.interruptTask!.bind(store.tasks);
    const tasks = new Proxy(store.tasks, {
      get(target, property) {
        if (property === "interruptTask")
          return (async (taskId, reason, opts) => {
            // The old worker already passed its lease precheck. Move ownership now,
            // immediately before the durable interruption checks its fence.
            await interrupt(taskId, "host sleep");
            await store.tasks.transitionTask(taskId, ["interrupted"], "queued");
            replacement = await store.tasks.claimNext({
              ownerId: "resumed",
              now: new Date(),
              scopesBusy: [],
            });
            try {
              return await interrupt(taskId, reason, opts);
            } catch (error) {
              fenced = (error as { code?: string }).code === "lease_lost";
              throw error;
            }
          }) satisfies NonNullable<TaskStore["interruptTask"]>;
        const value = Reflect.get(target, property);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    const wrapped = new Proxy(store, {
      get(target, property) {
        if (property === "tasks") return tasks;
        const value = Reflect.get(target, property);
        return typeof value === "function" ? value.bind(target) : value;
      },
    }) as AssistantStore;
    await store.tasks.createTask({
      taskId: "run",
      scopeId: "chat",
      kind: "work",
      payload: { pin: "original" },
    });
    const runner = new SingleProcessTaskRunner({ store: wrapped, pollMs: 1 });
    const handle = await runner.startWorker({
      execute: async () => {
        throw new TaskInterruptedError({
          code: "SLEEP",
          message: "old worker",
        });
      },
    });
    try {
      await waitFor(() => fenced, "stale interruption fence");
      await handle.stop();
      expect((await store.tasks.getTask("run"))?.status).toBe("running");
      expect((await store.tasks.getTask("run"))?.attemptCount).toBe(2);
      const current = replacement as ClaimedTask | null;
      expect(current).not.toBeNull();
      await store.tasks.updateProgress(
        "run",
        { live: true },
        { leaseToken: current!.lease.leaseToken },
      );
      expect((await store.tasks.getTask("run"))?.progress).toEqual({
        live: true,
      });
    } finally {
      await handle.stop();
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });
}
