// The gate wait used to be UNBOUNDED, which made the one mistake the
// serialization rule invites — a `transaction()` callback that awaits a
// root-store call — a silent, permanent hang: the call queues behind the
// transaction it is running inside, and that transaction cannot commit until
// the callback returns. A third-party host saw a request that never came back
// and nothing in a log to read. The watchdog turns it into an error naming its
// own cause, and these tests pin both halves: the mistake fails fast, and an
// honest caller that merely waits its turn is untouched.
import { describe, expect, it } from "bun:test";
import { SqliteAssistantStore } from "../src/index.js";

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

/** The rejection's `code`, or undefined if the promise resolved. */
async function codeOf(promise: Promise<unknown>): Promise<string | undefined> {
  try {
    await promise;
    return undefined;
  } catch (err) {
    return (err as { code?: string }).code;
  }
}

describe("SqliteAssistantStore — bounded transaction gate", () => {
  it("fails a root claimNext issued from inside a callback, fast", async () => {
    const store = new SqliteAssistantStore(":memory:", {
      transactionGateTimeoutMs: 50,
    });
    try {
      const started = Date.now();
      const code = await codeOf(
        store.transaction(async (tx) => {
          await tx.conversations.createChat({ id: "chat-tx" });
          await store.tasks.claimNext({
            ownerId: "worker-1",
            now: new Date(),
            scopesBusy: [],
          });
        }),
      );
      expect(code).toBe("transaction_gate_timeout");
      // Fast, not eventually: the point of the bound is that the caller learns
      // about it while the request that made the mistake is still alive.
      expect(Date.now() - started).toBeLessThan(1_000);
      // And the gate is not poisoned by the caller that gave up.
      await store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-after" });
      });
      expect(await store.conversations.getChat("chat-after")).not.toBeNull();
    } finally {
      store.close();
    }
  });

  it("fails a root write issued from inside a callback (whenFree's wait is bounded too)", async () => {
    const store = new SqliteAssistantStore(":memory:", {
      transactionGateTimeoutMs: 50,
    });
    try {
      await store.conversations.createChat({ id: "chat-a" });
      const code = await codeOf(
        store.transaction(async (tx) => {
          await tx.conversations.listChats();
          // An ordinary write waits out a transaction it is not part of
          // (Phase 1.6) — including, wrongly, this one.
          await store.conversations.updateChat("chat-a", { title: "renamed" });
        }),
      );
      expect(code).toBe("transaction_gate_timeout");
      // The write never happened: it gave up before its turn came.
      expect((await store.conversations.getChat("chat-a"))?.title).not.toBe(
        "renamed",
      );
    } finally {
      store.close();
    }
  });

  it("leaves honest serialized callers alone", async () => {
    const store = new SqliteAssistantStore(":memory:", {
      transactionGateTimeoutMs: 50,
    });
    try {
      // Two callers that overlap and both finish inside the budget: the
      // watchdog has to be cleared when a caller's turn comes, not left armed
      // to fire at whatever the queue is doing 50ms later.
      const opened = deferred();
      const release = deferred();
      const first = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-1" });
        opened.resolve();
        await release.promise;
      });
      await opened.promise;
      const second = store.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-2" });
      });
      const write = store.conversations.createChat({ id: "chat-3" });
      release.resolve();
      await first;
      await second;
      await write;
      expect(await store.conversations.getChat("chat-1")).not.toBeNull();
      expect(await store.conversations.getChat("chat-2")).not.toBeNull();
      expect(await store.conversations.getChat("chat-3")).not.toBeNull();
    } finally {
      store.close();
    }
  });
});
