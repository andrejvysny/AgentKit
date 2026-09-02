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
  const notesServer = {
    id: "mcp-1",
    alias: "notes",
    transport: { kind: "stdio", command: "notes-server" },
    createdAt: CONFIG_AT,
    updatedAt: CONFIG_AT,
  } as const;

  it("survives the rollback of an unrelated transaction it never joined", async () => {
    // Sharing a CONNECTION is not sharing a UNIT OF WORK. This store used to
    // ask the driver "are you in a transaction?" — true for ANY holder — and
    // join whatever it found: `create()` returned a record, and the host
    // transaction that happened to be open then threw and took the row with it.
    // The same `txDepth`-for-ownership mistake the aggregate store already paid
    // for, one object over.
    const store = new SqliteAssistantStore(":memory:");
    const configs = new SqliteMcpServerConfigStore(store.database);
    try {
      const opened = deferred();
      const mayFail = deferred();
      const doomed = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-rolled-back" });
        opened.resolve();
        await mayFail.promise;
        throw new Error("the transaction changed its mind");
      });
      await opened.promise;

      // Issued by someone with no part in that transaction, on the same handle.
      const created = configs.create({ ...notesServer });
      await drainLoop();
      // Still waiting: under the bug it had already run inside the open
      // transaction, which is what made it collateral damage below.
      expect(await configs.list()).toEqual([]);

      mayFail.resolve();
      await expect(doomed).rejects.toThrow("the transaction changed its mind");
      expect((await created).id).toBe("mcp-1");

      // THE HEADLINE: the rollback took only the writes that belonged to it.
      expect((await configs.list()).map((row) => row.id)).toEqual(["mcp-1"]);
      expect(await store.conversations.getChat("chat-rolled-back")).toBeNull();
    } finally {
      store.close();
    }
  });

  it("commits behind a transaction that commits, in arrival order", async () => {
    const store = new SqliteAssistantStore(":memory:");
    const configs = new SqliteMcpServerConfigStore(store.database);
    try {
      const order: string[] = [];
      const opened = deferred();
      const release = deferred();
      const outer = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-1" });
        order.push("outer");
        opened.resolve();
        await release.promise;
      });
      await opened.promise;

      const created = configs.create({ ...notesServer });
      // Issued AFTER the config write, so it must see the row: the transaction
      // reporting what it can read is the only ordering evidence that is not an
      // artifact of when a promise happened to resolve.
      const queued = store.transaction(async () => {
        order.push((await configs.list()).length === 1 ? "sees" : "misses");
      });
      await drainLoop();
      expect(order).toEqual(["outer"]);

      release.resolve();
      await outer;
      await created;
      await queued;

      // The config write took its slot BEFORE the queued transaction did, and
      // the queue kept that order.
      expect(order).toEqual(["outer", "sees"]);
      expect((await configs.list()).map((row) => row.id)).toEqual(["mcp-1"]);
      expect(await store.conversations.getChat("chat-1")).not.toBeNull();
    } finally {
      store.close();
    }
  });
});

describe("SqliteAssistantStore — a ROOT write against an open transaction", () => {
  it("queues the write behind the transaction instead of enlisting it", async () => {
    // The residual hole after the owner gate landed: `transaction()` callers
    // stopped joining each other, but an unrelated caller's SYNCHRONOUS port
    // write still flattened into whatever transaction happened to be open — and
    // died with its rollback. Two HTTP requests in one event loop is all it
    // takes: one mid-`transaction()`, the other renaming a chat.
    const store = new SqliteAssistantStore(":memory:");
    try {
      await store.conversations.createChat({ id: "chat-a" });

      const opened = deferred();
      const mayFail = deferred();
      const doomed = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-rolled-back" });
        opened.resolve();
        await mayFail.promise;
        throw new Error("the transaction changed its mind");
      });
      await opened.promise;

      // Issued on the ROOT store, by someone with no part in that transaction.
      const rename = store.conversations.updateChat("chat-a", {
        title: "renamed",
      });
      await drainLoop();
      // Still waiting: under the bug it had already run inside the open
      // transaction, which is what made it collateral damage below.
      expect(
        (await store.conversations.getChat("chat-a"))?.title,
      ).toBeUndefined();

      mayFail.resolve();
      await expect(doomed).rejects.toThrow("the transaction changed its mind");
      await rename;

      // THE HEADLINE: the rollback took only the writes that belonged to it.
      expect((await store.conversations.getChat("chat-a"))?.title).toBe(
        "renamed",
      );
      expect(await store.conversations.getChat("chat-rolled-back")).toBeNull();
    } finally {
      store.close();
    }
  });

  it("still rolls a tx-view write back with the transaction it belongs to", async () => {
    // The other half of the same rule: waiting is for STRANGERS. A write made
    // through the view the callback was handed is part of that unit of work and
    // must die with it — if it queued instead, it would wait on a transaction
    // only its own callback can end.
    const store = new SqliteAssistantStore(":memory:");
    try {
      await expect(
        store.transaction(async (tx) => {
          await tx.conversations.createChat({ id: "chat-inner" });
          await tx.providers.upsertProvider({
            id: "p1",
            label: "P",
            kind: "openai-compatible",
            baseUrl: "http://localhost:1234",
            defaultModel: "m",
            enabled: true,
          });
          await tx.settings.updateSettings({ defaultModel: "m" });
          throw new Error("outer changed its mind");
        }),
      ).rejects.toThrow("outer changed its mind");

      expect(await store.conversations.getChat("chat-inner")).toBeNull();
      expect(await store.providers.listProviders()).toEqual([]);
      expect((await store.settings.getSettings()).defaultModel).toBeUndefined();
    } finally {
      store.close();
    }
  });

  it("does not let a transaction queued behind the same gate swallow the write", async () => {
    // The microtask trap in the obvious implementation: `await ready()` then
    // `withTx()` are two ticks, and a transaction already queued on the gate
    // runs its BEGIN in between — so the write flattens into a stranger after
    // all, and the stranger's rollback erases it. Three callers, arriving in
    // this order, is the shape that catches it.
    const store = new SqliteAssistantStore(":memory:");
    try {
      await store.conversations.createChat({ id: "chat-a" });

      const opened = deferred();
      const release = deferred();
      const first = store.transaction(async (tx) => {
        await tx.conversations.listChats();
        opened.resolve();
        await release.promise;
      });
      await opened.promise;

      const rename = store.conversations.updateChat("chat-a", {
        title: "renamed",
      });
      const second = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-second" });
        throw new Error("second changed its mind");
      });
      await drainLoop();

      release.resolve();
      await first;
      await rename;
      await expect(second).rejects.toThrow("second changed its mind");

      expect((await store.conversations.getChat("chat-a"))?.title).toBe(
        "renamed",
      );
      expect(await store.conversations.getChat("chat-second")).toBeNull();
    } finally {
      store.close();
    }
  });
});

describe("SqliteAssistantStore — the gate runs its callers in ARRIVAL order", () => {
  it("lands a root write that later transactions keep arriving in front of", async () => {
    // The wait used to be a retry loop — "is the connection free yet?" re-asked
    // after each settle — and a retry loop is not a queue. Every
    // `transaction()` issued AFTER this write had already chained its own
    // `.then` onto the promise the write was waiting on, so it opened its BEGIN
    // first and the write found the connection busy again, forever. Three
    // overlapping callers is all it takes, and every turn-critical write
    // (`appendEvents`, `appendMessage`, `transitionTask`) is on this path.
    const store = new SqliteAssistantStore(":memory:", {
      transactionGateTimeoutMs: 250,
    });
    try {
      await store.conversations.createChat({ id: "chat-a" });

      let churning = true;
      let announced = false;
      const opened = deferred();
      const churn = async (): Promise<void> => {
        while (churning) {
          await store.transaction(async (tx) => {
            await tx.conversations.listChats();
            if (!announced) {
              announced = true;
              opened.resolve();
            }
            // Held across real time, the way any host transaction that awaits
            // anything holds it.
            await new Promise((resolve) => setTimeout(resolve, 2));
          });
        }
      };
      const loops = [churn(), churn(), churn()];
      await opened.promise;

      // Issued while a transaction is open, by someone with no part in it.
      const rename = store.conversations
        .updateChat("chat-a", { title: "renamed" })
        .then(
          () => "landed",
          (err: unknown) => (err as { code?: string }).code ?? "threw",
        );
      // Longer than the gate budget: a starved write has run out by now, and
      // the churn is still arriving in front of it.
      await new Promise((resolve) => setTimeout(resolve, 400));
      churning = false;
      await Promise.all(loops);

      // THE HEADLINE: arrival order, not "whoever noticed the gate free first".
      expect(await rename).toBe("landed");
      expect((await store.conversations.getChat("chat-a"))?.title).toBe(
        "renamed",
      );
    } finally {
      store.close();
    }
  });

  it("interleaves transactions and root writes in the order they were issued", async () => {
    // Each transaction reports the title it can see, so the sequence of reads
    // IS the run order: a write that ran late shows up as a transaction that
    // read the value before it.
    const store = new SqliteAssistantStore(":memory:");
    try {
      await store.conversations.createChat({ id: "chat-a" });
      const seen: (string | undefined)[] = [];
      const opened = deferred();
      const release = deferred();

      // Holds the gate so everything below has to queue rather than run on
      // arrival — the whole question is what order the queue keeps.
      const holder = store.transaction(async (tx) => {
        await tx.conversations.listChats();
        opened.resolve();
        await release.promise;
      });
      await opened.promise;

      // Issued write, transaction, write, transaction — in that order.
      const first = store.conversations.updateChat("chat-a", {
        title: "first",
      });
      const readA = store.transaction(async (tx) => {
        seen.push((await tx.conversations.getChat("chat-a"))?.title);
      });
      const second = store.conversations.updateChat("chat-a", {
        title: "second",
      });
      const readB = store.transaction(async (tx) => {
        seen.push((await tx.conversations.getChat("chat-a"))?.title);
      });
      await drainLoop();

      release.resolve();
      await Promise.all([holder, first, readA, second, readB]);

      expect(seen).toEqual(["first", "second"]);
      expect((await store.conversations.getChat("chat-a"))?.title).toBe(
        "second",
      );
    } finally {
      store.close();
    }
  });
});
