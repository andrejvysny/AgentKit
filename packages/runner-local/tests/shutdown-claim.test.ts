import { expect, it } from "bun:test";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import type { AssistantStore, TaskExecution, TaskStore } from "@agentkit/host";
import { SingleProcessTaskRunner } from "../src/index.js";

function gate(): { promise: Promise<void>; resolve: () => void } {
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

function attemptStatus(
  store: MemoryAssistantStore | SqliteAssistantStore,
  taskId: string,
): string | undefined {
  if (store instanceof MemoryAssistantStore)
    return [...store.tasks.attempts.values()].find(
      (attempt) => attempt.taskId === taskId,
    )?.status;
  return (
    store.database
      .query("SELECT status FROM task_attempts WHERE task_id = $taskId")
      .get({ $taskId: taskId }) as { status: string } | null
  )?.status;
}

for (const adapter of ["memory", "sqlite"] as const) {
  for (const shutdownMode of ["drain", "cancel", "interrupt"] as const) {
    it(`${adapter}: ${shutdownMode} waits for a blocked claim without executing it`, async () => {
      const store =
        adapter === "memory"
          ? new MemoryAssistantStore()
          : new SqliteAssistantStore(":memory:");
      const entered = gate();
      const release = gate();
      let leaseToken = "";
      let releases = 0;
      let executions = 0;
      const wrapped = wrapStore(store, {
        claimNext: async (input) => {
          const claimed = await store.tasks.claimNext(input);
          leaseToken = claimed!.lease.leaseToken;
          entered.resolve();
          await release.promise;
          return claimed;
        },
        releaseLease: async (token) => {
          releases++;
          return store.tasks.releaseLease(token);
        },
      });
      await store.tasks.createTask({
        taskId: "late",
        scopeId: "scope",
        kind: "work",
        payload: {},
      });
      const runner = new SingleProcessTaskRunner({
        store: wrapped,
        shutdownMode,
      });
      const handle = await runner.startWorker({
        execute: async () => {
          executions++;
        },
      });
      try {
        await entered.promise;
        let stopped = false;
        const stopping = handle.stop().then(() => {
          stopped = true;
        });
        await Promise.resolve();
        expect(stopped).toBe(false);
        release.resolve();
        await stopping;
        expect(executions).toBe(0);
        expect(releases).toBe(1);
        expect((await store.tasks.getTask("late"))?.status).toBe(
          shutdownMode === "cancel" ? "cancelled" : "interrupted",
        );
        expect(attemptStatus(store, "late")).toBe("cancelled");
        await expect(
          store.tasks.renewLease(leaseToken, 1_000),
        ).rejects.toMatchObject({ code: "lease_lost" });
      } finally {
        release.resolve();
        await handle.stop();
        if (store instanceof SqliteAssistantStore) store.close();
      }
    });
  }

  for (const stop of [false, true]) {
    it(`${adapter}: ${stop ? "shutdown" : "request"} interruption retains ownership until executor cleanup settles`, async () => {
      const store =
        adapter === "memory"
          ? new MemoryAssistantStore()
          : new SqliteAssistantStore(":memory:");
      const entered = gate();
      const aborted = gate();
      const cleanup = gate();
      const renewed = gate();
      let execution: TaskExecution | undefined;
      const calls: string[] = [];
      let interruptions = 0;
      const wrapped = wrapStore(store, {
        renewLease: async (token, ttl) => {
          const lease = await store.tasks.renewLease(token, ttl);
          if (abortedSignal()) renewed.resolve();
          return lease;
        },
        interruptTask: async (...args) => {
          interruptions++;
          return store.tasks.interruptTask!(...args);
        },
      });
      function abortedSignal(): boolean {
        return execution?.signal.aborted ?? false;
      }
      for (const id of ["active", "next"])
        await store.tasks.createTask({
          taskId: id,
          scopeId: "scope",
          kind: "work",
          payload: {},
        });
      const runner = new SingleProcessTaskRunner({
        store: wrapped,
        interruptionMode: "after_execution",
        shutdownMode: "interrupt",
        heartbeatMs: 1,
      });
      const handle = await runner.startWorker(
        {
          execute: async (input) => {
            calls.push(input.taskId);
            if (input.taskId !== "active") return;
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
            // Swallowing abort must not turn a host interruption into completion.
          },
        },
        { concurrency: 1 },
      );
      let stopping: Promise<void> | undefined;
      try {
        await entered.promise;
        if (stop) stopping = handle.stop();
        else
          await runner.requestInterrupt("active", {
            code: "SLEEP",
            message: "Host sleep",
          });
        await aborted.promise;
        await renewed.promise;
        expect(interruptions).toBe(0);
        expect((await store.tasks.getTask("active"))?.status).toBe("running");
        expect(attemptStatus(store, "active")).toBe("running");
        expect(runner.scopeLock.activeTask("scope")).toBe("active");
        expect(calls).toEqual(["active"]);
        await expect(runner.resume("active")).rejects.toMatchObject({
          code: "invalid_task_transition",
        });
        expect(
          await store.tasks.claimNext({
            ownerId: "replacement",
            now: new Date(),
            scopesBusy: runner.scopeLock.busyScopes(),
          }),
        ).toBeNull();
        cleanup.resolve();
        await (stopping ?? handle.stop());
        expect(interruptions).toBe(1);
        expect((await store.tasks.getTask("active"))?.status).toBe(
          "interrupted",
        );
        expect(
          JSON.parse((await store.tasks.getTask("active"))!.error!).code,
        ).toBe(stop ? "HOST_SHUTDOWN" : "SLEEP");
        expect(attemptStatus(store, "active")).toBe("cancelled");
        expect((await store.tasks.getTask("next"))?.status).toBe("queued");
        await expect(
          store.tasks.renewLease(execution!.leaseToken, 1_000),
        ).rejects.toMatchObject({ code: "lease_lost" });
      } finally {
        cleanup.resolve();
        await handle.stop();
        if (store instanceof SqliteAssistantStore) store.close();
      }
    });
  }
}

for (const cancel of [false, true]) {
  it(`${cancel ? "cancel" : "interrupt"} before execution skips the worker after a blocked lease check`, async () => {
    const store = new MemoryAssistantStore();
    const entered = gate();
    const release = gate();
    let executions = 0;
    const wrapped = wrapStore(store, {
      renewLease: async (token, ttl) => {
        await store.tasks.renewLease(token, ttl);
        entered.resolve();
        await release.promise;
        return store.tasks.renewLease(token, ttl);
      },
    });
    await store.tasks.createTask({
      taskId: "preflight",
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
    const runner = new SingleProcessTaskRunner({
      store: wrapped,
      interruptionMode: "after_execution",
    });
    const handle = await runner.startWorker({
      execute: async () => {
        executions++;
      },
    });
    try {
      await entered.promise;
      if (cancel) await runner.requestCancel("preflight");
      else
        await runner.requestInterrupt("preflight", {
          code: "SLEEP",
          message: "Host sleep",
        });
      release.resolve();
      await handle.stop();
      expect(executions).toBe(0);
      expect((await store.tasks.getTask("preflight"))?.status).toBe(
        cancel ? "cancelled" : "interrupted",
      );
      expect(attemptStatus(store, "preflight")).toBe("cancelled");
    } finally {
      release.resolve();
      await handle.stop();
    }
  });
}

it("user cancellation wins over a deferred typed interruption after cleanup", async () => {
  const store = new MemoryAssistantStore();
  const entered = gate();
  const aborted = gate();
  const cleanup = gate();
  await store.tasks.createTask({
    taskId: "cancel",
    scopeId: "scope",
    kind: "work",
    payload: {},
  });
  const runner = new SingleProcessTaskRunner({
    store,
    interruptionMode: "after_execution",
  });
  const handle = await runner.startWorker({
    execute: async ({ signal }) => {
      entered.resolve();
      await new Promise<void>((resolve) =>
        signal.addEventListener(
          "abort",
          () => {
            aborted.resolve();
            resolve();
          },
          { once: true },
        ),
      );
      await cleanup.promise;
      throw signal.reason;
    },
  });
  try {
    await entered.promise;
    await runner.requestInterrupt("cancel", {
      code: "SLEEP",
      message: "Host sleep",
    });
    await aborted.promise;
    await runner.requestCancel("cancel");
    cleanup.resolve();
    await handle.stop();
    expect((await store.tasks.getTask("cancel"))?.status).toBe("cancelled");
    expect(attemptStatus(store, "cancel")).toBe("cancelled");
  } finally {
    cleanup.resolve();
    await handle.stop();
  }
});

for (const shutdownMode of ["drain", "cancel", "interrupt"] as const) {
  it(`${shutdownMode} late-claim settlement cannot overwrite a replacement lease`, async () => {
    const store = new MemoryAssistantStore();
    const entered = gate();
    const release = gate();
    let calls = 0;
    const wrapped = wrapStore(store, {
      claimNext: async (input) => {
        const claimed = await store.tasks.claimNext(input);
        entered.resolve();
        await release.promise;
        return claimed;
      },
    });
    await store.tasks.createTask({
      taskId: "fenced",
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
    const runner = new SingleProcessTaskRunner({
      store: wrapped,
      shutdownMode,
    });
    const handle = await runner.startWorker({
      execute: async () => {
        calls++;
      },
    });
    try {
      await entered.promise;
      const stopping = handle.stop();
      await store.tasks.interruptTask("fenced", "replaced");
      await store.tasks.transitionTask("fenced", ["interrupted"], "queued");
      const replacement = await store.tasks.claimNext({
        ownerId: "replacement",
        now: new Date(),
        scopesBusy: [],
      });
      release.resolve();
      await stopping;
      expect(calls).toBe(0);
      expect((await store.tasks.getTask("fenced"))?.status).toBe("running");
      await store.tasks.updateProgress(
        "fenced",
        { live: true },
        { leaseToken: replacement!.lease.leaseToken },
      );
      expect((await store.tasks.getTask("fenced"))?.progress).toEqual({
        live: true,
      });
    } finally {
      release.resolve();
      await handle.stop();
    }
  });
}
