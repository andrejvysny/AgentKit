import { describe, expect, it } from "bun:test";
import {
  describeMcpServerConfigStoreConformance,
  MCP_CONFIG_UPDATED_AT,
} from "@agentkit/testing";
import { MemoryMcpServerConfigStore } from "../src/index.js";

/** Pinned, so the suite can assert `update` stamps from the STORE's clock. */
const clock = {
  now: () => new Date(MCP_CONFIG_UPDATED_AT),
  nowIso: () => MCP_CONFIG_UPDATED_AT,
};

describeMcpServerConfigStoreConformance({
  name: "MemoryMcpServerConfigStore",
  create: async () => ({ store: new MemoryMcpServerConfigStore({ clock }) }),
  test: { describe, it, expect },
});
