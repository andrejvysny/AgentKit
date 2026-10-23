import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  SqliteAssistantStore,
  SharedSqliteConnectionStore,
  SharedSqlitePromptTemplateStore,
} from "../src/index.js";
import type { ConnectionRecord, PromptTemplate } from "@agentkit/contracts";
test("connection records enforce compare-and-swap and template revisions are immutable", async () => {
  const assistant = new SqliteAssistantStore(":memory:");
  try {
    const connections = new SharedSqliteConnectionStore(assistant.database);
    const record: ConnectionRecord = {
      id: "one",
      accountId: "account",
      connectorId: "custom-mcp",
      revision: 1,
      status: "disconnected",
      config: { alias: "one", transport: { kind: "stdio", command: "one" } },
    };
    await connections.put(record, null);
    await expect(connections.put(record, null)).rejects.toMatchObject({
      code: "mcp_stale_connection",
    });
    await connections.put({ ...record, revision: 2 }, 1);
    await expect(connections.put(record, 1)).rejects.toMatchObject({
      code: "mcp_stale_connection",
    });
    expect((await connections.get("one"))?.revision).toBe(2);
    const second = new SharedSqliteConnectionStore(assistant.database);
    expect((await second.list())[0]?.revision).toBe(2);
    const templates = new SharedSqlitePromptTemplateStore(assistant.database);
    const template: PromptTemplate = {
      id: "review",
      revision: "1",
      description: "test",
      argumentsSchema: { type: "object" },
      messages: [{ role: "user", content: "Review" }],
    };
    await templates.put(template);
    await expect(templates.put(template)).rejects.toThrow();
    expect(await templates.get("review", "1")).toEqual(template);
    await connections.delete("one");
    expect(await connections.get("one")).toBeNull();
    await templates.delete("review", "1");
    expect(await templates.list()).toEqual([]);
  } finally {
    assistant.close();
  }
});

test("managed templates and connections survive actual SQLite close/reopen", async () => {
  const directory = await mkdtemp(join(tmpdir(), "agentkit-managed-content-"));
  const path = join(directory, "state.sqlite");
  let assistant = new SqliteAssistantStore(path);
  const template: PromptTemplate = {
    id: "review",
    revision: "1",
    description: "Review",
    argumentsSchema: { type: "object" },
    messages: [{ role: "user", content: "Review exact version" }],
  };
  const connection: ConnectionRecord = {
    id: "github",
    accountId: "account",
    connectorId: "custom-mcp",
    revision: 1,
    status: "disconnected",
    config: {
      alias: "github",
      transport: { kind: "stdio", command: "server" },
    },
  };
  try {
    await new SharedSqlitePromptTemplateStore(assistant.database).put(template);
    await new SharedSqliteConnectionStore(assistant.database).put(
      connection,
      null,
    );
    assistant.close();
    assistant = new SqliteAssistantStore(path);
    const templates = new SharedSqlitePromptTemplateStore(assistant.database);
    const connections = new SharedSqliteConnectionStore(assistant.database);
    expect(await templates.get("review", "1")).toEqual(template);
    await expect(templates.put(template)).rejects.toThrow();
    expect(await connections.get("github")).toEqual(connection);
    await expect(
      connections.put({ ...connection, revision: 2 }, 0),
    ).rejects.toMatchObject({ code: "mcp_stale_connection" });
    await connections.put({ ...connection, revision: 2 }, 1);
    await templates.delete(template.id, template.revision);
    expect(await templates.get(template.id, template.revision)).toBeNull();
    expect(await templates.list()).toEqual([]);
    assistant.close();
    assistant = new SqliteAssistantStore(path);
    expect(
      (await new SharedSqliteConnectionStore(assistant.database).get("github"))
        ?.revision,
    ).toBe(2);
    const reopenedTemplates = new SharedSqlitePromptTemplateStore(
      assistant.database,
    );
    expect(
      await reopenedTemplates.get(template.id, template.revision),
    ).toBeNull();
    expect(await reopenedTemplates.list()).toEqual([]);
    await expect(
      reopenedTemplates.put({
        ...template,
        messages: [{ role: "user", content: "replacement" }],
      }),
    ).rejects.toThrow();
  } finally {
    assistant.close();
    await rm(directory, { recursive: true, force: true });
  }
});

test("managed optional table names do not bypass SQLite ownership validation", async () => {
  for (const table of [
    "prompt_templates",
    "connector_connections",
    "foreign_table",
  ]) {
    const directory = await mkdtemp(join(tmpdir(), "agentkit-managed-shape-"));
    const path = join(directory, "state.sqlite");
    const assistant = new SqliteAssistantStore(path);
    assistant.database.exec(`CREATE TABLE ${table} (foreign_data TEXT)`);
    assistant.close();
    try {
      expect(() => new SqliteAssistantStore(path)).toThrow(
        "refusing to change this database",
      );
      const inspected = new Database(path);
      try {
        expect(
          inspected
            .query(`SELECT sql FROM sqlite_master WHERE name = ?`)
            .get(table),
        ).toEqual({ sql: `CREATE TABLE ${table} (foreign_data TEXT)` });
      } finally {
        inspected.close();
      }
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  }
});
