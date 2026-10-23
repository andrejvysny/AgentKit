import { describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SCHEMA_V8, SqliteAssistantStore } from "../src/index.js";
import { SCHEMA_V9_PROVIDER_CONTINUATIONS } from "../src/provider-continuation-schema.js";

async function branch(store: SqliteAssistantStore) {
  const chat = await store.conversations.createChat({});
  const root = await store.conversations.appendMessage({
    chatId: chat.id,
    role: "user",
    content: "Question",
  });
  const old = await store.conversations.appendMessage({
    chatId: chat.id,
    role: "assistant",
    content: "quartz archived",
  });
  const child = await store.conversations.appendMessage({
    chatId: chat.id,
    role: "user",
    content: "quartz child",
  });
  const live = await store.conversations.appendMessage({
    chatId: chat.id,
    role: "assistant",
    content: "Live",
    parentMessageId: root.id,
  });
  return { chatId: chat.id, root, old, child, live };
}

describe("message archive durability", () => {
  it("rolls every archive flag back when a later descendant update fails", async () => {
    const store = new SqliteAssistantStore(":memory:");
    try {
      const tree = await branch(store);
      const before = await store.conversations.listTree!(tree.chatId, {
        includeArchived: true,
      });
      store.database.exec(
        `CREATE TRIGGER reject_archive BEFORE UPDATE OF archived ON messages WHEN OLD.id = '${tree.child.id}' BEGIN SELECT RAISE(ABORT, 'injected archive failure'); END;`,
      );
      await expect(
        store.conversations.archiveBranch!(tree.old.id),
      ).rejects.toThrow("injected archive failure");
      expect(
        await store.conversations.listTree!(tree.chatId, {
          includeArchived: true,
        }),
      ).toEqual(before);
      expect(await store.conversations.searchMessages!("quartz")).toHaveLength(
        2,
      );
    } finally {
      store.close();
    }
  });

  it("retains archive flags and audit content across reopen", async () => {
    const directory = mkdtempSync(join(tmpdir(), "agentkit-archive-"));
    const path = join(directory, "store.sqlite");
    let store = new SqliteAssistantStore(path);
    try {
      const tree = await branch(store);
      await store.conversations.archiveBranch!(tree.old.id);
      store.close();
      store = new SqliteAssistantStore(path);
      expect(await store.conversations.getMessage(tree.old.id)).toMatchObject({
        archived: true,
        content: "quartz archived",
      });
      expect(
        (await store.conversations.listTree!(tree.chatId)).map(
          (message) => message.id,
        ),
      ).toEqual([tree.root.id, tree.live.id]);
      expect(await store.conversations.searchMessages!("quartz")).toEqual([]);
      await expect(
        store.conversations.activatePath(tree.old.id),
      ).rejects.toMatchObject({ code: "not_found" });
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });

  it("upgrades a v9 database to v10 without changing existing message content", async () => {
    const directory = mkdtempSync(join(tmpdir(), "agentkit-archive-migrate-"));
    const path = join(directory, "store.sqlite");
    const database = new Database(path);
    database.exec(SCHEMA_V8);
    database.exec(SCHEMA_V9_PROVIDER_CONTINUATIONS);
    database.exec("PRAGMA user_version = 9");
    database.run(
      "INSERT INTO chats (id, created_at, updated_at) VALUES ('chat', '2026-01-01', '2026-01-01')",
    );
    database.run(
      "INSERT INTO messages (id, chat_id, role, content, order_key, created_at) VALUES ('root', 'chat', 'user', 'Original', 1, '2026-01-01')",
    );
    database.close();
    const store = new SqliteAssistantStore(path);
    try {
      expect((await store.conversations.getMessage("root"))?.content).toBe(
        "Original",
      );
      expect((await store.conversations.getMessage("root"))?.archived).not.toBe(
        true,
      );
      const version = store.database.query("PRAGMA user_version").get() as {
        user_version: number | bigint;
      };
      expect(Number(version.user_version)).toBe(10);
    } finally {
      store.close();
      rmSync(directory, { recursive: true, force: true });
    }
  });
});
