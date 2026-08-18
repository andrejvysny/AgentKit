// File-backed behavior the shared conformance suite (which only ever uses
// ":memory:") cannot exercise: persistence across process boundaries, schema
// re-application, and a direct look at the partial unique index's SQL DDL and
// runtime behavior, bypassing the store class entirely.
import { describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import type { Changes } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SqliteAssistantStore } from "../src/index.js";

/** A fresh temp dir per test, cleaned up (even on failure) once `fn` settles. */
function withTempDb(
  fn: (path: string) => void | Promise<void>,
): () => Promise<void> {
  return async () => {
    const dir = mkdtempSync(join(tmpdir(), "agentkit-reference-adapters-"));
    const path = join(dir, "store.sqlite");
    try {
      await fn(path);
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  };
}

describe("SqliteAssistantStore — file-backed specifics", () => {
  it(
    "persists data across close + reopen on the same file",
    withTempDb(async (path) => {
      const first = new SqliteAssistantStore(path);
      const chat = await first.conversations.createChat({ title: "Persisted chat" });
      const message = await first.conversations.appendMessage({
        chatId: chat.id,
        role: "user",
        content: "hello",
      });
      first.close();

      const second = new SqliteAssistantStore(path);
      try {
        const fetchedChat = await second.conversations.getChat(chat.id);
        expect(fetchedChat?.title).toBe("Persisted chat");
        const messages = await second.conversations.listMessages(chat.id);
        expect(messages.map((m) => m.id)).toEqual([message.id]);
      } finally {
        second.close();
      }
    }),
  );

  it(
    "re-applies SCHEMA_V1 idempotently when the same file is opened again",
    withTempDb(async (path) => {
      const first = new SqliteAssistantStore(path);
      await first.conversations.createChat({ id: "seed-chat" });
      first.close();

      let second: SqliteAssistantStore | undefined;
      expect(() => {
        second = new SqliteAssistantStore(path);
      }).not.toThrow();
      try {
        // The seed row from before reopening is still there — schema
        // re-application did not touch data, only re-declared structure.
        const chat = await second!.conversations.getChat("seed-chat");
        expect(chat?.id).toBe("seed-chat");
      } finally {
        second?.close();
      }
    }),
  );

  it(
    "enforces the partial unique index on proposals(scope_key, action_id) directly at the SQL layer",
    withTempDb(async (path) => {
      // Apply the schema via the store, then talk to the file directly with
      // a second raw connection — independent of any application-level
      // mapping code in SqliteProposalStore.
      const store = new SqliteAssistantStore(path);
      store.close();

      const raw = new Database(path);
      try {
        const indexRow = raw
          .query(
            `SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_proposals_action_id'`,
          )
          .get() as { sql: string } | undefined;
        expect(indexRow?.sql).toContain("scope_key");
        expect(indexRow?.sql).toContain("action_id");
        expect(indexRow?.sql).toContain("NOT IN");

        // bun-types' generic for Database.run does not model its own
        // documented single-object calling convention — see the identical
        // note on SqliteConnection.run in the store itself. Re-typing `raw`
        // (not extracting `raw.run` into a bare variable) keeps the method
        // call bound to the Database instance, which bun:sqlite's native
        // binding requires at runtime.
        const rawDb = raw as unknown as {
          run(sql: string, params: Record<string, string>): Changes;
        };
        const insertProposal = (id: string, status: string): void => {
          rawDb.run(
            `INSERT INTO proposals
               (id, chat_id, scope_key, action_id, tool_name, kind, risk, status, envelope, operations, warnings, truncated, created_at)
             VALUES
               ($id, 'chat-1', 'scope-raw', 'act-raw', 'tool', 'kind', 'low', $status, '{}', '[]', '[]', 0, $now)`,
            { $id: id, $status: status, $now: new Date().toISOString() },
          );
        };

        insertProposal("raw-1", "pending");
        let threw = false;
        try {
          insertProposal("raw-2", "pending");
        } catch (err) {
          threw = true;
          expect((err as { code?: string }).code).toBe("SQLITE_CONSTRAINT_UNIQUE");
        }
        expect(threw).toBe(true);

        // A rejected row's key is free — the index's WHERE clause excludes it.
        raw.run(`UPDATE proposals SET status = 'rejected' WHERE id = 'raw-1'`);
        expect(() => insertProposal("raw-3", "pending")).not.toThrow();
      } finally {
        raw.close();
      }
    }),
  );
});
