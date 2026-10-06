import assert from "node:assert/strict";
import { after, describe, it } from "node:test";
import {
  NodeSqliteAssistantStore,
  NodeSqliteMcpServerConfigStore,
} from "../packages/agentkit/dist/adapters-sqlite/node.js";
import {
  describeAssistantStoreConformance,
  describeMcpServerConfigStoreConformance,
  MCP_CONFIG_UPDATED_AT,
} from "../packages/agentkit/dist/testing/index.js";

// Watchdog tests intentionally unref their timers; keep the test process alive.
const keepAlive = setInterval(() => {}, 1000);
after(() => clearInterval(keepAlive));
function expect(value, negate = false) {
  const check = (predicate, message) =>
    assert.equal(predicate, !negate, message);
  return {
    get not() {
      return expect(value, !negate);
    },
    toBe(expected) {
      negate
        ? assert.notStrictEqual(value, expected)
        : assert.strictEqual(value, expected);
    },
    toEqual(expected) {
      negate
        ? assert.notDeepStrictEqual(value, expected)
        : assert.deepStrictEqual(value, expected);
    },
    toBeNull() {
      check(value === null, "expected null");
    },
    toBeDefined() {
      check(value !== undefined, "expected defined");
    },
    toBeUndefined() {
      check(value === undefined, "expected undefined");
    },
    toContain(expected) {
      check(value.includes(expected), "expected contained value");
    },
    toBeGreaterThan(expected) {
      check(value > expected, "expected greater value");
    },
    toHaveLength(expected) {
      assert.equal(value.length, expected);
    },
  };
}
const test = { describe, it, expect };
function harness(options = {}) {
  const store = new NodeSqliteAssistantStore(":memory:", options);
  return {
    store,
    capabilities: { atomicTransactions: true, search: true },
    close: () => store.close(),
  };
}
describeAssistantStoreConformance({
  name: "Node better-sqlite3",
  create: async () => harness(),
  createTuned: async ({ clock, aging, transactionGateTimeoutMs }) =>
    harness({ clock, ...aging, transactionGateTimeoutMs }),
  test,
});
describeMcpServerConfigStoreConformance({
  name: "Node better-sqlite3 config",
  create: async () => {
    const clock = {
      now: () => new Date(MCP_CONFIG_UPDATED_AT),
      nowIso: () => MCP_CONFIG_UPDATED_AT,
    };
    const store = new NodeSqliteMcpServerConfigStore(":memory:", { clock });
    return { store, close: () => store.close() };
  },
  test,
});
