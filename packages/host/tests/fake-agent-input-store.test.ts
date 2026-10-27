import { describe, expect, it } from "bun:test";
import { describeAgentInputStoreConformance } from "@agentkit/testing";
import { createHarness, FakeAgentInputStore } from "./fakes.js";

describeAgentInputStoreConformance({
  name: "FakeAgentInputStore",
  create: async () => {
    const { store, clock } = createHarness();
    store.inputs = new FakeAgentInputStore(clock);
    return { store, capabilities: { atomicTransactions: false } };
  },
  test: { describe, it, expect },
});
