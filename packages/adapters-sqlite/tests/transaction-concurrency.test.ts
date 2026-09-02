// Two INDEPENDENT callers overlapping in time on ONE store instance.
//
// `withAsyncTx` used to decide reentrancy off a bare depth counter, so "a
// transaction is open" was read as "MY transaction is open" and an unrelated
// caller's whole unit of work joined the open BEGIN — living or dying with it.
// Two shapes of that, both driven by hand here rather than hoped for:
//
//   D1 — two `transaction()` calls. The second returns "committed" while its
//        writes sit inside the first caller's transaction, and the first
//        caller's rollback erases them.
//   D2 — `claimNext` while an unrelated `transaction()` is open. The claim is
//        handed to a worker, the outer rolls back, the task is `queued` again
//        and the next claim hands the SAME task to a second worker.
//
// The fix serializes top-level async transactions per connection: a caller that
// arrives while one is open WAITS, and gets its own BEGIN and its own rollback
// blast radius. Writes made from INSIDE the running callback still join it,
// which the shared-handle config store below depends on.
import { describe, expect, it } from "bun:test";
import {
  SqliteAssistantStore,
  SqliteMcpServerConfigStore,
} from "../src/index.js";

interface Deferred {
  promise: Promise<void>;
  resolve: () => void;
}

function deferred(): Deferred {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

/**
 * Macrotask turns — enough for a caller that FLATTENS to run all the way to
 * completion before the test looks. Under the bug that is what happened; the
 * assertions below are only meaningful once the loop has been given that
 * chance.
 */
async function drainLoop(): Promise<void> {
  for (let i = 0; i < 5; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

const CONFIG_AT = "2026-01-01T00:00:00.000Z";

describe("SqliteAssistantStore — concurrent transaction() callers", () => {
  it("keeps the second caller's commit when the first caller rolls back", async () => {
    const store = new SqliteAssistantStore(":memory:");
    try {
      const opened = deferred();
      const mayFail = deferred();
      const first = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-a" });
        opened.resolve();
        await mayFail.promise;
        throw new Error("first caller changed its mind");
      });

      await opened.promise;
      const second = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-b" });
      });
      await drainLoop();

      mayFail.resolve();
      await expect(first).rejects.toThrow("first caller changed its mind");
      await second;

      // THE HEADLINE: the second caller's transaction was its own, so the
      // first caller's rollback cannot reach it.
      expect(await store.conversations.getChat("chat-b")).not.toBeNull();
      // And the failed caller's own write is gone — the half that always held.
      expect(await store.conversations.getChat("chat-a")).toBeNull();
    } finally {
      store.close();
    }
  });

  it("makes the waiting caller wait, instead of joining the open transaction", async () => {
    const store = new SqliteAssistantStore(":memory:");
    try {
      const order: string[] = [];
      const opened = deferred();
      const release = deferred();
      const first = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-a" });
        order.push("first");
        opened.resolve();
        await release.promise;
      });

      await opened.promise;
      const second = store.transaction(async (tx) => {
        order.push("second");
        // Reading the first caller's chat is what a JOINED caller could do;
        // serialized, this runs after that transaction committed.
        expect(await tx.conversations.getChat("chat-a")).not.toBeNull();
      });
      await drainLoop();

      // Still outside: the second caller has not run a single statement while
      // the first holds the connection.
      expect(order).toEqual(["first"]);

      release.resolve();
      await first;
      await second;
      expect(order).toEqual(["first", "second"]);
    } finally {
      store.close();
    }
  });

  it("flattens a nested transaction() on the view it handed the callback", async () => {
    const store = new SqliteAssistantStore(":memory:");
    try {
      // The owner token is what tells a nested call apart from a stranger's —
      // without it this would queue behind the transaction it is running
      // inside, which is a wait that can never end.
      await store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-outer" });
        await tx.transaction(async (nested) => {
          await nested.conversations.createChat({ id: "chat-nested" });
        });
      });
      expect(await store.conversations.getChat("chat-nested")).not.toBeNull();

      // And it is genuinely ONE transaction: the outer's throw takes the
      // nested writes with it, rather than the nested call having committed on
      // its own.
      await expect(
        store.transaction(async (tx) => {
          await tx.transaction(async (nested) => {
            await nested.conversations.createChat({ id: "chat-doomed" });
          });
          throw new Error("outer changed its mind");
        }),
      ).rejects.toThrow("outer changed its mind");
      expect(await store.conversations.getChat("chat-doomed")).toBeNull();
    } finally {
      store.close();
    }
  });
});

describe("SqliteTaskStore — claimNext against an unrelated open transaction", () => {
  it("does not roll the claim back with the transaction it did not belong to", async () => {
    const store = new SqliteAssistantStore(":memory:");
    try {
      await store.tasks.createTask({
        taskId: "t1",
        kind: "unit",
        scopeId: "scope-1",
        payload: {},
      });

      const opened = deferred();
      const mayFail = deferred();
      const outer = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-outer" });
        opened.resolve();
        await mayFail.promise;
        throw new Error("outer changed its mind");
      });
      await opened.promise;

      const claim = store.tasks.claimNext({
        ownerId: "worker-a",
        now: new Date(),
        scopesBusy: [],
      });
      await drainLoop();

      mayFail.resolve();
      await expect(outer).rejects.toThrow("outer changed its mind");

      const claimed = await claim;
      expect(claimed?.task.taskId).toBe("t1");
      // THE HEADLINE: the claim survived a rollback it was never part of. Under
      // the bug the row reverted to `queued` while the worker held a lease on
      // it, and the next claim below handed the same task out again.
      expect((await store.tasks.getTask("t1"))?.status).toBe("running");
      expect(
        await store.tasks.claimNext({
          ownerId: "worker-b",
          now: new Date(),
          scopesBusy: [],
        }),
      ).toBeNull();
    } finally {
      store.close();
    }
  });
});

describe("SqliteMcpServerConfigStore — the shared handle, under the gate", () => {
  it("commits a config write made inside transaction(), with a caller queued behind it", async () => {
    const store = new SqliteAssistantStore(":memory:");
    const configs = new SqliteMcpServerConfigStore(store.database);
    try {
      const order: string[] = [];
      const opened = deferred();
      const release = deferred();
      const outer = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-1" });
        // Another object over the SAME handle, writing synchronously: this is
        // the flatten the gate must not take away.
        await configs.create({
          id: "mcp-1",
          alias: "notes",
          transport: { kind: "stdio", command: "notes-server" },
          createdAt: CONFIG_AT,
          updatedAt: CONFIG_AT,
        });
        order.push("outer");
        opened.resolve();
        await release.promise;
      });

      await opened.promise;
      const queued = store.transaction(async () => {
        order.push("queued");
      });
      await drainLoop();
      expect(order).toEqual(["outer"]);

      release.resolve();
      await outer;
      await queued;

      expect(order).toEqual(["outer", "queued"]);
      expect((await configs.list()).map((row) => row.id)).toEqual(["mcp-1"]);
      expect(await store.conversations.getChat("chat-1")).not.toBeNull();
    } finally {
      store.close();
    }
  });
});
