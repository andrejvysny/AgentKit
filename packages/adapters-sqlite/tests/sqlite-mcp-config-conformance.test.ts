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
 * config write racing the assistant store's own `transaction()`.
 *
 * Sharing a connection is not sharing a unit of work. This store used to read
 * the driver's `Database.inTransaction` — true for ANY holder — and join
 * whatever transaction it found, which meant a stranger's rollback erased a
 * config write that had already been reported as created. It now takes a slot
 * on the same write queue every other write on this handle takes, and the
 * corollary is the aggregate's own documented one: a config write AWAITED from
 * inside a `transaction()` callback is waiting on a transaction only that
 * callback can end.
 */
describe("SqliteMcpServerConfigStore — against the assistant store's transaction", () => {
  const notesServer = {
    id: "mcp-1",
    alias: "notes",
    transport: { kind: "stdio", command: "notes-server" },
    createdAt: MCP_CONFIG_UPDATED_AT,
    updatedAt: MCP_CONFIG_UPDATED_AT,
  } as const;

  it("keeps a config write a stranger's transaction rolled back", async () => {
    const assistant = new SqliteAssistantStore(":memory:");
    const configs = new SqliteMcpServerConfigStore(assistant.database, {
      clock,
    });
    try {
      let release!: () => void;
      const released = new Promise<void>((resolve) => {
        release = resolve;
      });
      let opened!: () => void;
      const isOpen = new Promise<void>((resolve) => {
        opened = resolve;
      });

      const doomed = assistant.transaction(async (tx) => {
        await tx.conversations.createChat({ id: "chat-1" });
        opened();
        await released;
        throw new Error("caller changed its mind");
      });
      await isOpen;

      const created = configs.create({ ...notesServer });
      release();
      await expect(doomed).rejects.toThrow("caller changed its mind");
      await created;

      // THE HEADLINE: the config row is still there. Under the join it was
      // created, acknowledged, and then erased by a transaction it had nothing
      // to do with.
      expect((await configs.list()).map((row) => row.id)).toEqual(["mcp-1"]);
      expect(await assistant.conversations.getChat("chat-1")).toBeNull();
    } finally {
      assistant.close();
    }
  });

  it("refuses a config write awaited from inside the callback, fast", async () => {
    // The mistake the queue makes visible instead of hanging on — the same rule
    // (and the same error) a root `store.conversations.updateChat(...)` in
    // there gets. A callback that needs a config write does it before or after
    // its transaction, not inside it.
    const assistant = new SqliteAssistantStore(":memory:", {
      transactionGateTimeoutMs: 50,
    });
    const configs = new SqliteMcpServerConfigStore(assistant.database, {
      clock,
    });
    try {
      let code: string | undefined;
      try {
        await assistant.transaction(async (tx) => {
          await tx.conversations.createChat({ id: "chat-1" });
          await configs.create({ ...notesServer });
        });
      } catch (err) {
        code = (err as { code?: string }).code;
      }
      expect(code).toBe("transaction_gate_timeout");
      // Neither half landed, and the gate is not poisoned by the caller that
      // gave up.
      expect(await configs.list()).toEqual([]);
      expect(await assistant.conversations.getChat("chat-1")).toBeNull();
      await configs.create({ ...notesServer });
      expect((await configs.list()).map((row) => row.id)).toEqual(["mcp-1"]);
    } finally {
      assistant.close();
    }
  });
});
