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
import { SCHEMA_VERSION, SqliteAssistantStore } from "../src/index.js";

/** A fresh temp dir per test, cleaned up (even on failure) once `fn` settles. */
function withTempDb(
  fn: (path: string) => void | Promise<void>,
): () => Promise<void> {
  return async () => {
    const dir = mkdtempSync(join(tmpdir(), "agentkit-adapters-sqlite-"));
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
      const chat = await first.conversations.createChat({
        title: "Persisted chat",
      });
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
    "re-applies SCHEMA_V8 idempotently when the same file is opened again",
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
    "refuses a database written by a different schema version instead of layering tables over it",
    withTempDb(async (path) => {
      const first = new SqliteAssistantStore(path);
      await first.conversations.createChat({ id: "v1-era-chat" });
      first.close();

      // Rewind the stamp to simulate a database an older build wrote. The rows
      // are still there, so this is NOT the "fresh file" case — the adapter
      // ships no migrations and must say so rather than guess.
      const raw = new Database(path);
      raw.exec("PRAGMA user_version = 1;");
      raw.close();

      let code: string | undefined;
      let message = "";
      try {
        new SqliteAssistantStore(path);
      } catch (err) {
        code = (err as { code?: string }).code;
        message = (err as Error).message;
      }
      expect(code).toBe("sqlite_schema_version");
      expect(message).toContain("no migrations");
    }),
  );

  it(
    "refuses a database stamped with the PREVIOUS schema version rather than migrating it",
    withTempDb(async (path) => {
      // The concrete upgrade case, not a synthetic one: v7 is the version the
      // build before this one wrote, and it is exactly the file a developer
      // still has on disk. No migration ships, so the only correct answer is
      // the typed refusal — silently layering v8's DDL over a v7 file would
      // leave `proposals.claimed_at` missing and every apply claim failing at
      // runtime instead of at open.
      const first = new SqliteAssistantStore(path);
      await first.conversations.createChat({ id: "v7-era-chat" });
      first.close();

      // The literal 7, not `SCHEMA_VERSION - 1`: this pins the ONE version a
      // real dev database out there carries, and it has to keep failing the
      // open after the next bump too.
      const raw = new Database(path);
      raw.exec("PRAGMA user_version = 7;");
      raw.close();
      expect(SCHEMA_VERSION).toBeGreaterThan(7);

      let code: string | undefined;
      try {
        new SqliteAssistantStore(path);
      } catch (err) {
        code = (err as { code?: string }).code;
      }
      expect(code).toBe("sqlite_schema_version");
    }),
  );

  it(
    "keeps the FTS index correct across a close + reopen, and the guarded backfill does not double-index",
    withTempDb(async (path) => {
      // The one thing ":memory:" cannot show: the schema DDL is re-applied in
      // full on every open, backfill statement included. An unguarded backfill
      // would insert a second copy of every message into the index here, and
      // the only symptom would be duplicate hits.
      const first = new SqliteAssistantStore(path);
      const chat = await first.conversations.createChat({});
      const message = await first.conversations.appendMessage({
        chatId: chat.id,
        role: "user",
        content: "a persisted magnetometer reading",
      });
      first.close();

      const second = new SqliteAssistantStore(path);
      try {
        const hits =
          await second.conversations.searchMessages?.("magnetometer");
        expect(hits?.map((h) => h.messageId)).toEqual([message.id]);
        // Still live: the triggers survived the re-application too.
        const added = await second.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "another magnetometer, written after the reopen",
        });
        expect(
          (await second.conversations.searchMessages?.("magnetometer"))?.length,
        ).toBe(2);
        await second.conversations.deleteChat(chat.id);
        expect(
          (await second.conversations.searchMessages?.("magnetometer"))?.length,
        ).toBe(0);
        expect(added.orderKey).toBe(2);
      } finally {
        second.close();
      }
    }),
  );

  it(
    "declares the v8 index and column the schema bump exists for",
    withTempDb(async (path) => {
      // Read off the FILE, not off the DDL string: what the store actually
      // applied is the only thing a stale dev database or a half-applied DDL
      // could disagree with. `idx_messages_run` serves lastMessageOfRun's
      // (chat_id, run_id, ORDER BY depth DESC, order_key DESC) lookup, the
      // whole ORDER BY included so no temp b-tree is needed; `proposals.claimed_at`
      // is the reconcile window's stamp.
      const store = new SqliteAssistantStore(path);
      store.close();
      const raw = new Database(path);
      try {
        const index = raw
          .query(
            `SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_messages_run'`,
          )
          .get() as { sql: string } | null;
        expect(index?.sql).toContain(
          "messages(chat_id, run_id, depth, order_key)",
        );
        const columns = (
          raw.query(`PRAGMA table_info(proposals)`).all() as { name: string }[]
        ).map((column) => column.name);
        expect(columns).toContain("claimed_at");
      } finally {
        raw.close();
      }
    }),
  );

  it(
    "stamps user_version on a fresh database so the next open recognises it",
    withTempDb(async (path) => {
      const store = new SqliteAssistantStore(path);
      store.close();
      const raw = new Database(path);
      try {
        const row = raw.query(`PRAGMA user_version`).get() as {
          user_version: number;
        };
        expect(row.user_version).toBe(SCHEMA_VERSION);
      } finally {
        raw.close();
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
          expect((err as { code?: string }).code).toBe(
            "SQLITE_CONSTRAINT_UNIQUE",
          );
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
