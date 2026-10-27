import { expect, it } from "bun:test";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import type { AssistantStore, TaskExecution, TaskStore } from "@agentkit/host";
import { SingleProcessTaskRunner } from "../src/index.js";

function gate(): { promise: Promise<void>; resolve(): void } {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function wrapStore(
  store: AssistantStore,
  overrides: Partial<TaskStore>,
): AssistantStore {
  const tasks = new Proxy(store.tasks, {
    get(target, property) {
      const value =
        Reflect.get(overrides, property) ?? Reflect.get(target, property);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
  return new Proxy(store, {
    get(target, property) {
      if (property === "tasks") return tasks;
      const value = Reflect.get(target, property);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}
const reason = { code: "SLEEP", message: "Host sleep" };

for (const adapter of ["memory", "sqlite"] as const) {
  it(`${adapter}: a queued interruption racing a claim retains the live lease through cleanup`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    const mutationEntered = gate();
    const releaseMutation = gate();
    const entered = gate();
    const aborted = gate();
    const cleanup = gate();
    let blocked = false;
    let interruptionWrites = 0;
    let execution: TaskExecution | undefined;
    async function blockFirstMutation(): Promise<void> {
      if (blocked) return;
      blocked = true;
      mutationEntered.resolve();
      await releaseMutation.promise;
    }
    const wrapped = wrapStore(store, {
      transitionTask: async (...args) => {
        await blockFirstMutation();
        return store.tasks.transitionTask(...args);
      },
      interruptTask: async (...args) => {
        interruptionWrites++;
        await blockFirstMutation();
        return store.tasks.interruptTask!(...args);
      },
    });
    await store.tasks.createTask({
      taskId: "race",
      scopeId: "scope",
      kind: "work",
      payload: { pin: "original" },
    });
    const runner = new SingleProcessTaskRunner({
      store: wrapped,
      interruptionMode: "after_execution",
    });
    const interrupting = runner.requestInterrupt("race", reason);
    await mutationEntered.promise;
    const handle = await runner.startWorker(
      {
        execute: async (input) => {
          execution = input;
          entered.resolve();
          await new Promise<void>((resolve) =>
            input.signal.addEventListener(
              "abort",
              () => {
                aborted.resolve();
                resolve();
              },
              { once: true },
            ),
          );
          await cleanup.promise;
        },
      },
      { concurrency: 1 },
    );
    try {
      await entered.promise;
      releaseMutation.resolve();
      await interrupting;
      await aborted.promise;
      expect(interruptionWrites).toBe(0);
      expect((await store.tasks.getTask("race"))?.status).toBe("running");
      await store.tasks.renewLease(execution!.leaseToken, 1_000);
      expect(runner.scopeLock.activeTask("scope")).toBe("race");
      cleanup.resolve();
      await handle.stop();
      expect(interruptionWrites).toBe(1);
      expect((await store.tasks.getTask("race"))?.status).toBe("interrupted");
      expect((await store.tasks.getTask("race"))?.payload).toEqual({
        pin: "original",
      });
      await expect(
        store.tasks.renewLease(execution!.leaseToken, 1_000),
      ).rejects.toMatchObject({ code: "lease_lost" });
    } finally {
      releaseMutation.resolve();
      cleanup.resolve();
      await interrupting;
      await handle.stop();
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });

  it(`${adapter}: running work without a local executor keeps its owner and lease`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    await store.tasks.createTask({
      taskId: "orphan",
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
    const claimed = await store.tasks.claimNext({
      ownerId: "native-owner",
      now: new Date(),
      scopesBusy: [],
    });
    const runner = new SingleProcessTaskRunner({
      store,
      interruptionMode: "after_execution",
    });
    try {
      await runner.requestInterrupt("orphan", reason);
      expect((await store.tasks.getTask("orphan"))?.status).toBe("running");
      await store.tasks.updateProgress(
        "orphan",
        { cleanup: "pending" },
        { leaseToken: claimed!.lease.leaseToken },
      );
      expect((await store.tasks.getTask("orphan"))?.progress).toEqual({
        cleanup: "pending",
      });
      await store.tasks.renewLease(claimed!.lease.leaseToken, 1_000);
    } finally {
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });

  it(`${adapter}: queued-only interruption parks an unclaimed task without an attempt`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    await store.tasks.createTask({
      taskId: "queued",
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
    const runner = new SingleProcessTaskRunner({
      store,
      interruptionMode: "after_execution",
    });
    try {
      await runner.requestInterrupt("queued", reason);
      expect((await store.tasks.getTask("queued"))?.status).toBe("interrupted");
      expect((await store.tasks.getTask("queued"))?.attemptCount).toBe(0);
      expect(
        await store.tasks.claimNext({
          ownerId: "replacement",
          now: new Date(),
          scopesBusy: [],
        }),
      ).toBeNull();
    } finally {
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });
  it(`${adapter}: interruption waits for a claimed task to register before parking it`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    const claimed = gate();
    const releaseClaim = gate();
    const attempted = gate();
    const cleanup = gate();
    let settled = false;
    let calls = 0;
    const wrapped = wrapStore(store, {
      claimNext: async (input) => {
        const task = await store.tasks.claimNext(input);
        claimed.resolve();
        await releaseClaim.promise;
        return task;
      },
      transitionTask: async (...args) => {
        try {
          return await store.tasks.transitionTask(...args);
        } finally {
          attempted.resolve();
        }
      },
      interruptTask: async (...args) => {
        try {
          return await store.tasks.interruptTask!(...args);
        } finally {
          attempted.resolve();
        }
      },
    });
    await store.tasks.createTask({
      taskId: "pending",
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
    const runner = new SingleProcessTaskRunner({
      store: wrapped,
      interruptionMode: "after_execution",
    });
    const handle = await runner.startWorker(
      {
        execute: async () => {
          calls++;
          await cleanup.promise;
        },
      },
      { concurrency: 1 },
    );
    let interrupting: Promise<void> | undefined;
    try {
      await claimed.promise;
      interrupting = runner.requestInterrupt("pending", reason).then(() => {
        settled = true;
      });
      await attempted.promise;
      await Promise.resolve();
      expect(settled).toBe(false);
      expect((await store.tasks.getTask("pending"))?.status).toBe("running");
      releaseClaim.resolve();
      await interrupting;
      cleanup.resolve();
      await handle.stop();
      expect(calls).toBeLessThanOrEqual(1);
      expect((await store.tasks.getTask("pending"))?.status).toBe(
        "interrupted",
      );
    } finally {
      releaseClaim.resolve();
      cleanup.resolve();
      await interrupting;
      await handle.stop();
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });
}
