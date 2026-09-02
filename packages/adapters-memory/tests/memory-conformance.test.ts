import { describe, expect, it } from "bun:test";
import { describeAssistantStoreConformance } from "@agentkit/testing";
import { MemoryAssistantStore } from "../src/index.js";

describeAssistantStoreConformance({
  name: "MemoryAssistantStore",
  create: async () => ({
    store: new MemoryAssistantStore(),
    // Documented in the class doc on MemoryAssistantStore: transaction(fn)
    // just runs fn(this) — no BEGIN/COMMIT/ROLLBACK, so a throw after some
    // writes leaves those writes in place. The atomicity test skips this
    // adapter instead of failing it.
    // Search IS implemented here — a case-insensitive scan rather than an
    // index, which is the right trade for a store whose whole history is
    // already in memory. The flag exists for an adapter that ships no
    // `searchMessages` at all.
    capabilities: { atomicTransactions: false, search: true },
  }),
  createTuned: async ({ clock, aging, transactionGateTimeoutMs }) => ({
    store: new MemoryAssistantStore({
      clock,
      ...aging,
      ...(transactionGateTimeoutMs === undefined
        ? {}
        : { transactionGateTimeoutMs }),
    }),
    // Search IS implemented here — a case-insensitive scan rather than an
    // index, which is the right trade for a store whose whole history is
    // already in memory. The flag exists for an adapter that ships no
    // `searchMessages` at all.
    capabilities: { atomicTransactions: false, search: true },
  }),
  test: { describe, it, expect },
});
