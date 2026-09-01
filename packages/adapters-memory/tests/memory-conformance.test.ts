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
    capabilities: { atomicTransactions: false },
  }),
  createTuned: async ({ clock, aging }) => ({
    store: new MemoryAssistantStore({ clock, ...aging }),
    capabilities: { atomicTransactions: false },
  }),
  test: { describe, it, expect },
});
