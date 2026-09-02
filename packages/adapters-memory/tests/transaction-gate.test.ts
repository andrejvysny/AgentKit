// The memory adapter's half of the transaction contract, in the shapes the
// shared conformance suite grades both adapters on — kept here as well because
// this store is where the two used to disagree in OPPOSITE directions:
// `transaction()` handed its callback `this`, so a nested `tx.transaction(...)`
// queued behind the callback already holding the queue (a deadlock), while a
// root `store.tasks.claimNext()` from inside a callback sailed straight through
// the open unit. Sqlite answered both the other way round. A host that develops
// against this store and deploys against that one has to get the same answer.
import { describe, expect, it } from "bun:test";
import { MemoryAssistantStore } from "../src/index.js";

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

describe("MemoryAssistantStore — bounded transaction gate", () => {
  it("fails a root claimNext issued from inside a callback, fast", async () => {
    const store = new MemoryAssistantStore({ transactionGateTimeoutMs: 50 });
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
    expect(Date.now() - started).toBeLessThan(1_000);
    // The gate is not poisoned by the caller that gave up.
    await store.transaction(async (tx) => {
      await tx.conversations.createChat({ id: "chat-after" });
    });
    expect(await store.conversations.getChat("chat-after")).not.toBeNull();
  });

  it("runs a claim made through the tx view inside the open unit", async () => {
    const store = new MemoryAssistantStore({ transactionGateTimeoutMs: 50 });
    const claimed = await store.transaction(async (tx) => {
      await tx.tasks.createTask({
        taskId: "task-1",
        kind: "test.kind",
        scopeId: "scope-1",
        payload: {},
      });
      // Through `tx`, so it belongs to this unit and must NOT wait for it.
      return tx.tasks.claimNext({
        ownerId: "worker-1",
        now: new Date(),
        scopesBusy: [],
      });
    });
    expect(claimed?.task.taskId).toBe("task-1");
  });

  it("makes an unrelated claimNext wait for an open transaction", async () => {
    const store = new MemoryAssistantStore();
    await store.tasks.createTask({
      taskId: "task-1",
      kind: "test.kind",
      scopeId: "scope-1",
      payload: {},
    });
    const opened = deferred();
    const release = deferred();
    const order: string[] = [];
    const first = store.transaction(async (tx) => {
      await tx.conversations.createChat({ id: "chat-1" });
      order.push("transaction");
      opened.resolve();
      await release.promise;
    });
    await opened.promise;
    // A worker's claim, from outside: it is a stranger to the open unit and
    // takes its turn after it, exactly as it does over sqlite.
    const claim = store.tasks
      .claimNext({ ownerId: "worker-1", now: new Date(), scopesBusy: [] })
      .then((result) => {
        order.push("claim");
        return result;
      });
    for (let i = 0; i < 5; i += 1) {
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    expect(order).toEqual(["transaction"]);

    release.resolve();
    await first;
    expect((await claim)?.task.taskId).toBe("task-1");
    expect(order).toEqual(["transaction", "claim"]);
  });
});
