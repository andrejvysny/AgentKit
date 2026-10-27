import { describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Database } from "bun:sqlite";
import { SqliteAssistantStore } from "../src/index.js";

function withTempDb(fn: (path: string) => Promise<void>): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "agent-inputs-"));
  return fn(join(dir, "store.db")).finally(() =>
    rmSync(dir, { recursive: true, force: true }),
  );
}

function rawExec(path: string, sql: string): void {
  const db = new Database(path);
  try {
    db.exec(sql);
  } finally {
    db.close();
  }
}

function rawUserVersion(path: string): number {
  const db = new Database(path);
  try {
    return (db.query("PRAGMA user_version").get() as { user_version: number })
      .user_version;
  } finally {
    db.close();
  }
}

const input = {
  inputId: "in-1",
  sessionId: "chat-1",
  kind: "follow_up" as const,
  content: { text: "hi" },
  contentHash: "h",
};

describe("agent_inputs optional table", () => {
  it("is recreated on reopen without a schema bump", async () => {
    await withTempDb(async (path) => {
      const first = new SqliteAssistantStore(path);
      await first.inputs.createInput(input);
      first.close();
      expect(rawUserVersion(path)).toBe(10);

      rawExec(path, "DROP TABLE agent_inputs");
      const reopened = new SqliteAssistantStore(path);
      try {
        expect(await reopened.inputs.getInput("in-1")).toBeUndefined();
        const { created } = await reopened.inputs.createInput(input);
        expect(created).toBe(true);
      } finally {
        reopened.close();
      }
      expect(rawUserVersion(path)).toBe(10);

      // Ownership checks still pass on a third open with the table present.
      const third = new SqliteAssistantStore(path);
      try {
        expect((await third.inputs.getInput("in-1"))?.status).toBe("pending");
      } finally {
        third.close();
      }
    });
  });

  it("refuses a foreign table under the same name", async () => {
    await withTempDb(async (path) => {
      new SqliteAssistantStore(path).close();
      rawExec(
        path,
        "DROP TABLE agent_inputs; CREATE TABLE agent_inputs (input_id TEXT PRIMARY KEY, extra TEXT)",
      );
      let code: string | undefined;
      try {
        new SqliteAssistantStore(path).close();
      } catch (err) {
        code = (err as { code?: string }).code;
      }
      expect(code).toBe("sqlite_schema_integrity");
    });
  });
});
