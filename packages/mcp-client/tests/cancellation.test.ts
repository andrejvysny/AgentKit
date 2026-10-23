import { describe, expect, it } from "bun:test";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import {
  ErrorCode,
  McpError as SdkMcpError,
} from "@modelcontextprotocol/sdk/types.js";
import {
  McpClientManager,
  type McpServerConfig,
  type McpTransportFactory,
} from "../src/index.js";
import {
  buildFakeServer,
  createSecretStore,
  InMemoryHarness,
} from "./helpers.js";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function bounded<T>(promise: Promise<T>): Promise<T> {
  let timer!: ReturnType<typeof setTimeout>;
  try {
    return await Promise.race([
      promise,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("Cancellation did not settle promptly")),
          300,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function config(): McpServerConfig {
  return {
    alias: "notes",
    transport: { kind: "stdio", command: "fixture" },
    resilience: {
      maxConnectAttempts: 1,
      connectTimeoutMs: 1000,
      requestTimeoutMs: 1000,
      connectBackoffBaseMs: 1000,
      connectBackoffMaxMs: 1000,
      reconnectMaxAttempts: 1,
    },
  };
}

describe("MCP cancellation around connection waits", () => {
  it("does not open a transport for a call already cancelled", async () => {
    let attempts = 0;
    const manager = new McpClientManager(
      {
        secrets: createSecretStore(),
        transportFactory: async () => {
          attempts++;
          throw new Error("No transport should be built");
        },
      },
      [config()],
    );
    const abort = new AbortController();
    abort.abort();
    await expect(
      manager.callTool("mcp.notes.echo", {}, { signal: abort.signal }),
    ).rejects.toMatchObject({ code: "mcp_request_aborted" });
    expect(attempts).toBe(0);
    await manager.dispose();
  });

  it("cancels request retry backoff without reconnecting or replaying the tool", async () => {
    const called = deferred<void>();
    const harness = new InMemoryHarness({
      notes: () =>
        buildFakeServer("notes", [
          {
            name: "echo",
            handler: () => {
              called.resolve();
              throw new SdkMcpError(ErrorCode.ConnectionClosed, "closed");
            },
          },
        ]),
    });
    const manager = new McpClientManager(
      { secrets: createSecretStore(), transportFactory: harness.factory },
      [config()],
    );
    const abort = new AbortController();
    const call = manager.callTool(
      "mcp.notes.echo",
      {},
      { signal: abort.signal },
    );
    await called.promise;
    await Bun.sleep(10);
    abort.abort();
    await expect(bounded(call)).rejects.toMatchObject({
      code: "mcp_request_aborted",
    });
    expect(harness.connects("notes")).toBe(1);
    await manager.dispose();
    await harness.closeAll();
  });

  it("rejects slow noncooperative connect promptly and closes its late transport", async () => {
    const entered = deferred<void>();
    const release = deferred<void>();
    const closed = deferred<void>();
    let connectionSignal: AbortSignal | undefined;
    const server = buildFakeServer("notes", [{ name: "echo" }]);
    server.onclose = () => closed.resolve();
    const factory: McpTransportFactory = async (request) => {
      connectionSignal = request.signal;
      entered.resolve();
      await release.promise;
      const [client, remote] = InMemoryTransport.createLinkedPair();
      await server.connect(remote);
      return client;
    };
    const manager = new McpClientManager(
      { secrets: createSecretStore(), transportFactory: factory },
      [config()],
    );
    const abort = new AbortController();
    const call = manager.callTool(
      "mcp.notes.echo",
      {},
      { signal: abort.signal },
    );
    await entered.promise;
    abort.abort();
    await expect(bounded(call)).rejects.toMatchObject({
      code: "mcp_request_aborted",
    });
    expect(connectionSignal?.aborted).toBe(true);
    release.resolve();
    await bounded(closed.promise);
    expect(manager.isConnected("notes")).toBe(false);
    await manager.dispose();
    await server.close();
  });

  it("does not cancel a shared connection still awaited by another caller", async () => {
    const entered = deferred<void>();
    const release = deferred<void>();
    const harness = new InMemoryHarness({
      notes: () => buildFakeServer("notes", [{ name: "echo" }]),
    });
    let connectionSignal: AbortSignal | undefined;
    const manager = new McpClientManager(
      {
        secrets: createSecretStore(),
        transportFactory: async (request) => {
          connectionSignal = request.signal;
          entered.resolve();
          await release.promise;
          return harness.factory(request);
        },
      },
      [config()],
    );
    const abort = new AbortController();
    const first = manager.callTool(
      "mcp.notes.echo",
      { caller: 1 },
      { signal: abort.signal },
    );
    const second = manager.callTool("mcp.notes.echo", { caller: 2 });
    await entered.promise;
    abort.abort();
    await expect(bounded(first)).rejects.toMatchObject({
      code: "mcp_request_aborted",
    });
    expect(connectionSignal?.aborted).toBe(false);
    release.resolve();
    expect((await bounded(second)).text).toContain('"caller":2');
    expect(harness.connects("notes")).toBe(1);
    await manager.dispose();
    await harness.closeAll();
  });
});
