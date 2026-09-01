import { describe, expect, it } from "bun:test";
import { ConversationService } from "../src/index.js";
import { FakeAssistantStore, createTestClock, createTestIds } from "./fakes.js";

/**
 * `ConversationService.deleteChat` is the one operation in this package that
 * spans three stores, and the only thing it adds over calling them in a row is
 * the refusal: a chat with live work in it is not deleted, whatever the caller
 * asked for. So that is what these tests are about — the guard, what it is
 * scoped to, and that the cascade really does reach all three stores once the
 * guard passes.
 */
interface Fixture {
  store: FakeAssistantStore;
  service: ConversationService;
}

function setup(): Fixture {
  const store = new FakeAssistantStore(createTestClock(), createTestIds());
  return { store, service: new ConversationService({ store }) };
}

/** A chat with a message, a task in its scope, and a staged proposal. */
async function seedChat(store: FakeAssistantStore, chatId: string) {
  const chat = await store.conversations.createChat({ id: chatId });
  const message = await store.conversations.appendMessage({
    chatId: chat.id,
    role: "user",
    content: "hello",
  });
  const task = await store.tasks.createTask({
    taskId: `task-${chatId}`,
    kind: "chat.turn",
    // The convention TurnRunner writes with: a chat turn's scope IS the chat.
    scopeId: chat.id,
    payload: {},
  });
  const proposal = await store.proposals.create({
    id: `prp-${chatId}`,
    chatId: chat.id,
    scopeKey: "doc-1",
    toolName: "test.tool",
    kind: "test.kind",
    risk: "low",
    envelope: {},
    operations: [],
    warnings: [],
    truncated: false,
    createdAt: new Date().toISOString(),
  });
  return { chat, message, task, proposal };
}

describe("ConversationService.deleteChat", () => {
  it("refuses with chat_busy while a task in the chat's scope is running, and deletes nothing", async () => {
    const { store, service } = await setup();
    const { chat, task } = await seedChat(store, "chat-busy");
    await store.tasks.transitionTask(task.taskId, ["queued"], "running");

    let caught:
      | { code?: string; details?: Record<string, unknown> }
      | undefined;
    try {
      await service.deleteChat(chat.id);
    } catch (err) {
      caught = err as { code?: string; details?: Record<string, unknown> };
    }
    expect(caught?.code).toBe("chat_busy");
    // Names what is holding it, so a UI can point at the run instead of just
    // saying "try again".
    expect(caught?.details?.["taskIds"]).toEqual([task.taskId]);

    // Nothing was deleted on the way to the refusal.
    expect((await store.conversations.getChat(chat.id))?.id).toBe(chat.id);
    expect((await store.conversations.listMessages(chat.id)).length).toBe(1);
    expect(await store.tasks.getTask(task.taskId)).not.toBeNull();
    expect((await store.proposals.listByChat(chat.id)).length).toBe(1);
  });

  it("refuses on waiting_approval too — a decision nobody has made yet is live work", async () => {
    const { store, service } = await setup();
    const { chat, task } = await seedChat(store, "chat-waiting");
    await store.tasks.transitionTask(task.taskId, ["queued"], "running");
    await store.tasks.transitionTask(
      task.taskId,
      ["running"],
      "waiting_approval",
    );

    let code: string | undefined;
    try {
      await service.deleteChat(chat.id);
    } catch (err) {
      code = (err as { code?: string }).code;
    }
    expect(code).toBe("chat_busy");
  });

  it("deletes a chat whose only task is merely queued — nothing has been spent on it", async () => {
    const { store, service } = await setup();
    const { chat, task } = await seedChat(store, "chat-queued");
    await service.deleteChat(chat.id);
    expect(await store.conversations.getChat(chat.id)).toBeNull();
    expect(await store.tasks.getTask(task.taskId)).toBeNull();
  });

  it("cascades to messages, tasks and proposals when the chat is idle, and touches no neighbour", async () => {
    const { store, service } = await setup();
    const doomed = await seedChat(store, "chat-doomed");
    const neighbour = await seedChat(store, "chat-neighbour");
    // Finished work is not live work: it goes with the chat.
    await store.tasks.transitionTask(doomed.task.taskId, ["queued"], "running");
    await store.tasks.transitionTask(
      doomed.task.taskId,
      ["running"],
      "completed",
    );
    // A neighbouring chat with a RUNNING task must not make this delete fail:
    // the guard is scoped to one chat, not to the store.
    await store.tasks.transitionTask(
      neighbour.task.taskId,
      ["queued"],
      "running",
    );

    await service.deleteChat(doomed.chat.id);

    expect(await store.conversations.getChat(doomed.chat.id)).toBeNull();
    expect(
      (await store.conversations.listMessages(doomed.chat.id)).length,
    ).toBe(0);
    expect(await store.tasks.getTask(doomed.task.taskId)).toBeNull();
    expect(await store.proposals.get(doomed.proposal.id)).toBeNull();

    expect((await store.conversations.getChat(neighbour.chat.id))?.id).toBe(
      neighbour.chat.id,
    );
    expect(await store.tasks.getTask(neighbour.task.taskId)).not.toBeNull();
    expect(await store.proposals.get(neighbour.proposal.id)).not.toBeNull();
  });

  it("rejects an unknown chat rather than reporting a delete that never happened", async () => {
    const { service } = await setup();
    let code: string | undefined;
    try {
      await service.deleteChat("chat-nope");
    } catch (err) {
      code = (err as { code?: string }).code;
    }
    expect(code).toBe("not_found");
  });

  it("does the check and all three deletes inside ONE transaction", async () => {
    const { store, service } = await setup();
    const { chat } = await seedChat(store, "chat-tx");
    const before = store.transactions;
    await service.deleteChat(chat.id);
    // One, not four: a task read outside the transaction could go `running`
    // between the read and the delete, and the whole refusal would be a race.
    expect(store.transactions - before).toBe(1);
  });
});

describe("ConversationService archive", () => {
  it("archives and unarchives without touching anything else about the chat", async () => {
    const { store, service } = await setup();
    const chat = await store.conversations.createChat({
      id: "chat-archive",
      title: "Keeps its name",
      metadata: { pinned: true },
    });
    await store.conversations.appendMessage({
      chatId: chat.id,
      role: "user",
      content: "still here",
    });

    const archived = await service.archiveChat(chat.id);
    expect(archived.archived).toBe(true);
    expect(archived.title).toBe("Keeps its name");
    expect(archived.metadata).toEqual({ pinned: true });
    // Archiving is reversible and non-destructive — the messages stay.
    expect((await store.conversations.listMessages(chat.id)).length).toBe(1);
    // And it is a hide, not a delete: the chat still answers by id.
    expect((await store.conversations.listChats()).length).toBe(0);
    expect(
      (await store.conversations.listChats({ ids: [chat.id] })).length,
    ).toBe(1);

    const restored = await service.unarchiveChat(chat.id);
    expect(restored.archived).toBe(false);
    expect((await store.conversations.listChats()).length).toBe(1);
  });

  it("rejects archiving a chat that does not exist", async () => {
    const { service } = await setup();
    let code: string | undefined;
    try {
      await service.archiveChat("chat-nope");
    } catch (err) {
      code = (err as { code?: string }).code;
    }
    expect(code).toBe("not_found");
  });
});
