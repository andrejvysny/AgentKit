import { expect, test } from "bun:test";
import { ManagedMcpConfigStore, McpClientManager } from "../src/index.js";
import { MemoryMcpServerConfigStore } from "../../adapters-memory/src/memory-mcp-server-config-store.js";
import {
  InMemoryHarness,
  buildFakeServer,
  createSecretStore,
} from "./helpers.js";
test("stored edits invalidate live sessions and literal configuration never reaches durable storage", async () => {
  const secrets = createSecretStore();
  const raw = new MemoryMcpServerConfigStore();
  const harness = new InMemoryHarness({
    test: () => buildFakeServer("test", [{ name: "read" }]),
    renamed: () => buildFakeServer("renamed", [{ name: "read" }]),
  });
  const manager = new McpClientManager(
    { secrets, transportFactory: harness.factory },
    [],
  );
  const store = new ManagedMcpConfigStore(raw, manager, secrets);
  try {
    await store.create({
      id: "one",
      alias: "test",
      createdAt: "now",
      updatedAt: "now",
      transport: {
        kind: "stdio",
        command: "test",
        env: { TOKEN: "secret-value" },
      },
    });
    expect(JSON.stringify(await raw.list())).not.toContain("secret-value");
    await manager.listTools("test");
    expect(manager.isConnected("test")).toBe(true);
    await store.update("one", { enabled: false });
    expect(manager.isConnected("test")).toBe(false);
    await expect(manager.callTool("mcp.test.read", {})).rejects.toMatchObject({
      code: "mcp_not_connected",
    });
    await store.update("one", { alias: "renamed", enabled: true });
    expect(manager.aliases()).toEqual(["renamed"]);
    await manager.listTools("renamed");
    await store.delete("one");
    expect(manager.aliases()).toEqual([]);
  } finally {
    await manager.dispose();
  }
});
