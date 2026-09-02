import { describe, expect, it } from "bun:test";
import { describeAssistantStoreConformance } from "@agentkit/testing";
import { SqliteAssistantStore } from "../src/index.js";

describeAssistantStoreConformance({
  name: "SqliteAssistantStore (:memory:)",
  create: async () => {
    const store = new SqliteAssistantStore(":memory:");
    return {
      store,
      capabilities: { atomicTransactions: true, search: true },
      close: () => store.close(),
    };
  },
  createTuned: async ({ clock, aging, transactionGateTimeoutMs }) => {
    const store = new SqliteAssistantStore(":memory:", {
      clock,
      ...aging,
      ...(transactionGateTimeoutMs === undefined
        ? {}
        : { transactionGateTimeoutMs }),
    });
    return {
      store,
      capabilities: { atomicTransactions: true, search: true },
      close: () => store.close(),
    };
  },
  test: { describe, it, expect },
});
