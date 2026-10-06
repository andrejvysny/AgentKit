import { expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiProviderContinuation } from "@agentkit/contracts";
import {
  SCHEMA_V8,
  SCHEMA_VERSION,
  SqliteAssistantStore,
} from "../src/index.js";

async function temporary(run: (path: string) => Promise<void>): Promise<void> {
  const directory = mkdtempSync(join(tmpdir(), "agentkit-continuation-"));
  try {
    await run(join(directory, "store.sqlite"));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

function createV8(path: string): void {
  const database = new Database(path);
  try {
    database.exec(SCHEMA_V8);
    database.exec("PRAGMA user_version = 8");
    database.run(
      "INSERT INTO chats (id, title, created_at, updated_at) VALUES ('kept', 'Original chat', '2026-01-01', '2026-01-01')",
    );
  } finally {
    database.close();
  }
}

it("upgrades original v8 without changing baseline data and applies v9 on fresh open", async () => {
  await temporary(async (path) => {
    createV8(path);
    const store = new SqliteAssistantStore(path);
    try {
      expect(SCHEMA_VERSION).toBe(9);
      expect((await store.conversations.getChat("kept"))?.title).toBe(
        "Original chat",
      );
      expect(
        Number(
          (
            store.database.query("PRAGMA user_version").get() as {
              user_version: number | bigint;
            }
          ).user_version,
        ),
      ).toBe(9);
      const tables = store.database
        .query(
          "SELECT name FROM sqlite_master WHERE name LIKE 'provider_%' ORDER BY name",
        )
        .all() as { name: string }[];
      expect(tables.map((row) => row.name)).toContain("provider_continuations");
      expect(tables.map((row) => row.name)).toContain("provider_run_scopes");
    } finally {
      store.close();
    }
    const fresh = new SqliteAssistantStore(":memory:");
    try {
      expect(await fresh.continuations.getRunScope("missing")).toBeNull();
    } finally {
      fresh.close();
    }
  });
});

it("persists private state across close/reopen and rolls it back with its owner transaction", async () => {
  await temporary(async (path) => {
    const first = new SqliteAssistantStore(path);
    await first.conversations.createChat({ id: "chat-1" });
    await first.tasks.createTask({
      taskId: "run-1",
      kind: "chat.turn",
      scopeId: "chat-1",
      payload: { chatId: "chat-1" },
    });
    const claim = (await first.tasks.claimNext({
      ownerId: "worker",
      now: new Date(),
      scopesBusy: [],
    }))!;
    const fence = { taskId: "run-1", leaseToken: claim.lease.leaseToken };
    const anchor = await first.conversations.appendMessage({
      chatId: "chat-1",
      runId: "run-1",
      role: "assistant",
      content: "Answer",
    });
    const state: AiProviderContinuation = {
      version: 1,
      scope: {
        providerId: "provider-1",
        protocol: "responses",
        connectionId: "connection-1",
        generation: 1,
        chatId: "chat-1",
        branchId: "branch-1",
        model: "model-1",
      },
      messageCount: 1,
      messageDigest: "a".repeat(64),
      inputItems: [
        { type: "reasoning", encrypted_content: "opaque-ciphertext" },
      ],
    };
    try {
      await expect(
        first.transaction(async (tx) => {
          await tx.continuations!.bindRun("run-1", state.scope, fence);
          await tx.continuations!.put(
            { runId: "run-1", anchorMessageId: anchor.id, state },
            fence,
          );
          throw new Error("rollback");
        }),
      ).rejects.toThrow("rollback");
      expect(await first.continuations.getRunScope("run-1")).toBeNull();
      expect(await first.continuations.getByRun("run-1")).toBeNull();
      await first.continuations.bindRun("run-1", state.scope, fence);
      first.database.exec(`CREATE TRIGGER refuse_continuation_marker
        BEFORE UPDATE OF state_required ON provider_run_scopes
        BEGIN SELECT RAISE(ABORT, 'marker write failed'); END;`);
      await expect(
        first.continuations.put(
          { runId: "run-1", anchorMessageId: anchor.id, state },
          fence,
        ),
      ).rejects.toThrow("marker write failed");
      expect(await first.continuations.getByRun("run-1")).toBeNull();
      first.database.exec("DROP TRIGGER refuse_continuation_marker");
      await first.continuations.put(
        { runId: "run-1", anchorMessageId: anchor.id, state },
        fence,
      );
    } finally {
      first.close();
    }
    const reopened = new SqliteAssistantStore(path);
    try {
      expect(await reopened.continuations.getRunScope("run-1")).toEqual(
        state.scope,
      );
      expect(await reopened.continuations.getByRun("run-1")).toEqual({
        runId: "run-1",
        anchorMessageId: anchor.id,
        state,
      });
      const foreignAnchor = await reopened.conversations.appendMessage({
        chatId: "chat-1",
        runId: "foreign-run",
        role: "assistant",
        content: "Foreign run",
      });
      reopened.database.run(
        "UPDATE provider_continuations SET anchor_message_id = $anchor WHERE run_id = 'run-1'",
        { $anchor: foreignAnchor.id },
      );
      await expect(
        reopened.continuations.getByRun("run-1"),
      ).rejects.toMatchObject({
        code: "provider_continuation_scope_mismatch",
      });
      reopened.database.run(
        "DELETE FROM provider_continuations WHERE run_id = 'run-1'",
      );
      await expect(
        reopened.continuations.getByRun("run-1"),
      ).rejects.toMatchObject({
        code: "provider_continuation_state_missing",
      });
      await reopened.conversations.deleteChat("chat-1");
      await expect(
        reopened.continuations.getByRun("run-1"),
      ).rejects.toMatchObject({
        code: "provider_continuation_state_missing",
      });
    } finally {
      reopened.close();
    }
  });
});

it("rejects a spoofed v9 table layout without changing schema or data", async () => {
  await temporary(async (path) => {
    createV8(path);
    const database = new Database(path);
    database.exec(
      "CREATE TABLE provider_run_scopes (run_id TEXT PRIMARY KEY, wrong_column TEXT)",
    );
    database.exec(
      "CREATE TABLE provider_continuations (run_id TEXT PRIMARY KEY, wrong_column TEXT)",
    );
    database.exec("PRAGMA user_version = 9");
    const before = database
      .query("SELECT name, sql FROM sqlite_master ORDER BY name")
      .all();
    database.close();
    expect(() => new SqliteAssistantStore(path)).toThrow(
      "Unrecognized migrated AgentKit table",
    );
    const untouched = new Database(path);
    try {
      expect(
        untouched
          .query("SELECT name, sql FROM sqlite_master ORDER BY name")
          .all(),
      ).toEqual(before);
      expect(
        untouched.query("SELECT title FROM chats WHERE id = 'kept'").get(),
      ).toEqual({ title: "Original chat" });
      expect(untouched.query("PRAGMA user_version").get()).toEqual({
        user_version: 9,
      });
    } finally {
      untouched.close();
    }
  });
});
