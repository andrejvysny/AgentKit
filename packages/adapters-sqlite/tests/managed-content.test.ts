import { expect, test } from "bun:test";
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
