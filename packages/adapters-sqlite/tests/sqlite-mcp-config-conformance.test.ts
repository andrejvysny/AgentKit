import { describe, expect, it } from "bun:test";
import {
  describeMcpServerConfigStoreConformance,
  MCP_CONFIG_UPDATED_AT,
} from "@agentkit/testing";
import {
  SqliteAssistantStore,
  SqliteMcpServerConfigStore,
} from "../src/index.js";

/** Pinned, so the suite can assert `update` stamps from the STORE's clock. */
const clock = {
  now: () => new Date(MCP_CONFIG_UPDATED_AT),
  nowIso: () => MCP_CONFIG_UPDATED_AT,
};

describeMcpServerConfigStoreConformance({
  name: "SqliteMcpServerConfigStore (:memory:, own handle)",
  create: async () => {
    const store = new SqliteMcpServerConfigStore(":memory:", { clock });
    return { store, close: () => store.close() };
  },
  test: { describe, it, expect },
});

describeMcpServerConfigStoreConformance({
  name: "SqliteMcpServerConfigStore (over the assistant store's handle)",
  create: async () => {
    // The wiring a host actually uses: one database, one connection, one write
    // lock. The suite has to pass identically both ways, or "constructible over
    // the same handle" would be a claim nothing checks.
    const assistant = new SqliteAssistantStore(":memory:");
    return {
      store: new SqliteMcpServerConfigStore(assistant.database, { clock }),
      close: () => assistant.close(),
    };
  },
  test: { describe, it, expect },
});

/**
 * The half of "over the same handle" the conformance suite cannot reach: a
 * config write issued while the assistant store's own `transaction()` is open.
 *
 * `bun:sqlite` has no savepoints in this version, so a second `BEGIN IMMEDIATE`
 * on one handle raises rather than nesting — this store therefore flattens into
 * whatever transaction the handle already has, detected off the driver's own
 * `inTransaction` rather than a counter it keeps (the transaction belongs to the
 * OTHER object sharing the connection, which no local counter could see).
 */
describe("SqliteMcpServerConfigStore — inside the assistant store's transaction", () => {
  it("flattens into an open transaction instead of failing on a nested BEGIN", async () => {
    const assistant = new SqliteAssistantStore(":memory:");
    const configs = new SqliteMcpServerConfigStore(assistant.database, {
      clock,
    });
    try {
      await assistant.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-1" });
        await configs.create({
          id: "mcp-1",
          alias: "notes",
          transport: { kind: "stdio", command: "notes-server" },
          createdAt: MCP_CONFIG_UPDATED_AT,
          updatedAt: MCP_CONFIG_UPDATED_AT,
        });
      });
      expect((await configs.list()).map((row) => row.id)).toEqual(["mcp-1"]);
      expect(await assistant.conversations.getChat("chat-1")).not.toBeNull();
    } finally {
      assistant.close();
    }
  });

  it("rolls the config write back with the transaction that wrapped it", async () => {
    const assistant = new SqliteAssistantStore(":memory:");
    const configs = new SqliteMcpServerConfigStore(assistant.database, {
      clock,
    });
    try {
      await expect(
        assistant.transaction(async (tx) => {
          await configs.create({
            id: "mcp-1",
            alias: "notes",
            transport: { kind: "stdio", command: "notes-server" },
            createdAt: MCP_CONFIG_UPDATED_AT,
            updatedAt: MCP_CONFIG_UPDATED_AT,
          });
          await tx.conversations.getChat("chat-1");
          throw new Error("caller changed its mind");
        }),
      ).rejects.toThrow("caller changed its mind");
      // Flattening is not just "does not crash": the write joined the caller's
      // transaction, so the rollback takes it with it.
      expect(await configs.list()).toEqual([]);
    } finally {
      assistant.close();
    }
  });
});
