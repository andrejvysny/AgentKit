import { expect, it } from "bun:test";
import { SqliteAssistantStore } from "../src/index.js";

it("never exposes task status or events from a transaction that rolls back", async () => {
  const store = new SqliteAssistantStore(":memory:");
  try {
    const chat = await store.conversations.createChat({});
    const message = await store.conversations.appendMessage({
      chatId: chat.id,
      runId: "run",
      role: "assistant",
      content: "before",
    });
    await store.tasks.createTask({
      taskId: "run",
      kind: "unit",
      scopeId: "scope",
      payload: {},
    });
    const claim = (await store.tasks.claimNext({
      ownerId: "worker",
      now: new Date(),
      scopesBusy: [],
    }))!;
    let entered!: () => void;
    let release!: () => void;
    const started = new Promise<void>((resolve) => {
      entered = resolve;
    });
    const waiting = new Promise<void>((resolve) => {
      release = resolve;
    });
    const write = store
      .transaction(async (tx) => {
        await tx.conversations.updateMessage(
          message.id,
          { content: "uncommitted" },
          { taskId: "run", leaseToken: claim.lease.leaseToken },
        );
        await tx.tasks.appendEvents(
          "run",
          [
            {
              contractVersion: "1",
              eventId: "rolled-back",
              seq: 0,
              type: "run.completed",
              timestamp: new Date().toISOString(),
            },
          ],
          { leaseToken: claim.lease.leaseToken },
        );
        await tx.tasks.transitionTask(
          "run",
          ["running"],
          "completed",
          {},
          { leaseToken: claim.lease.leaseToken },
        );
        entered();
        await waiting;
        throw new Error("rollback terminal publication");
      })
      .catch((error) => error);
    await started;
    let readFinished = false;
    const read = Promise.all([
      store.tasks.getTask("run"),
      store.tasks.listEvents("run"),
      store.conversations.getMessage(message.id),
      store.conversations.listMessages(chat.id),
    ]).then((values) => {
      readFinished = true;
      return values;
    });
    await new Promise((resolve) => setTimeout(resolve, 5));
    const exposedBeforeCommit = readFinished;
    release();
    await write;
    const [task, events, fetched, messages] = await read;
    expect(exposedBeforeCommit).toBe(false);
    expect(task?.status).toBe("running");
    expect(events).toHaveLength(0);
    expect(fetched?.content).toBe("before");
    expect(messages[0]?.content).toBe("before");
  } finally {
    store.close();
  }
});
