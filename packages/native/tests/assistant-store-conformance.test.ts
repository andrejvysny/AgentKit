/**
 * The shared conformance drivers, run against `NativeAssistantStore` — the
 * same suites `MemoryAssistantStore` and `SqliteAssistantStore` pass, with
 * nothing weakened. The Rust store is a real SQLite engine behind the napi
 * binding, so it claims the strongest capabilities: real rollback and real
 * indexed search.
 */
import { describe, expect, it } from "bun:test";
import {
  describeAgentInputStoreConformance,
  describeAssistantStoreConformance,
  describeContextSummaryStoreConformance,
} from "@agentkit/testing";
import { NativeStore } from "../src/index.js";
import { NativeAssistantStore } from "../src/assistant-store.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);

const capabilities = { atomicTransactions: true, search: true };

const fresh = async () => {
  const native = await NativeStore.inMemory();
  return {
    store: new NativeAssistantStore({ store: native }),
    capabilities,
    // The harness close is synchronous by contract; closing the binding is
    // fire-and-forget on purpose — every test owns an isolated in-memory
    // database, so a pending close can never bleed into another test.
    close: () => {
      void native.close().catch(() => undefined);
    },
  };
};

describeAssistantStoreConformance({
  name: "NativeAssistantStore",
  create: fresh,
  createTuned: async ({ clock, transactionGateTimeoutMs, aging }) => {
    const native = await NativeStore.inMemory();
    if (transactionGateTimeoutMs !== undefined)
      native.setTransactionGateTimeout(transactionGateTimeoutMs);
    if (aging !== undefined) native.setTaskAging(aging);
    return {
      // The injected clock is threaded to the binding as the `now` every
      // time-taking op accepts, so engine stamps land on tuned time.
      store: new NativeAssistantStore({ store: native, clock }),
      capabilities,
      close: () => {
        void native.close().catch(() => undefined);
      },
    };
  },
  test: {
    describe,
    it,
    expect,
  },
});

describeAgentInputStoreConformance({
  name: "NativeAssistantStore",
  create: fresh,
  test: { describe, it, expect },
});

describeContextSummaryStoreConformance({
  name: "NativeAssistantStore",
  create: fresh,
  test: { describe, it, expect },
});
