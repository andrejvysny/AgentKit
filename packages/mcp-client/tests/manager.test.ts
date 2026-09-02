import { afterEach, describe, expect, it } from "bun:test";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import {
  McpClientManager,
  type McpError,
  type McpServerConfig,
  type McpTransportFactory,
} from "../src/index.js";
import {
  buildFakeServer,
  createSecretStore,
  createTestClock,
  deferred,
  InMemoryHarness,
  type TestClock,
} from "./helpers.js";

/** Fast, jitter-free resilience so the timings under test stay in single-digit ms. */
const FAST = {
  requestTimeoutMs: 60,
  connectTimeoutMs: 60,
  connectBackoffBaseMs: 1,
  connectBackoffMaxMs: 4,
  circuitOpenMs: 50,
} as const;

const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  while (cleanups.length > 0) await cleanups.pop()?.();
});

function setup(
  configs: McpServerConfig[],
  builders: Record<
    string,
    (attempt: number) => ReturnType<typeof buildFakeServer>
  >,
  secrets: Record<string, string> = {},
): { manager: McpClientManager; harness: InMemoryHarness; clock: TestClock } {
  const harness = new InMemoryHarness(builders);
  const clock = createTestClock();
  const manager = new McpClientManager(
    {
      secrets: createSecretStore(secrets),
      clock,
      transportFactory: harness.factory,
    },
    configs,
  );
  cleanups.push(async () => {
    await manager.dispose();
    await harness.closeAll();
  });
  return { manager, harness, clock };
}

const echoServer = (): ReturnType<typeof buildFakeServer> =>
  buildFakeServer("echo-server", [
    {
      name: "echo",
      description: "Echo the input",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
      annotations: { readOnlyHint: true },
      handler: (args) => ({
        content: [{ type: "text", text: `echo:${String(args?.["text"])}` }],
      }),
    },
    {
      name: "boom",
      handler: () => ({
        content: [{ type: "text", text: "the tool refused" }],
        isError: true,
      }),
    },
  ]);

describe("McpClientManager", () => {
  it("lists tools with canonical ids and the readOnlyHint effect mapping", async () => {
    const { manager } = setup(
      [
        {
          alias: "echo",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { echo: echoServer },
    );
    const tools = await manager.listTools("echo");
    expect(tools.map((t) => t.canonicalId)).toEqual([
      "mcp.echo.echo",
      "mcp.echo.boom",
    ]);
    expect(tools[0]?.effect).toBe("read");
    // No annotation at all ⇒ write, conservatively.
    expect(tools[1]?.effect).toBe("write");
    expect(tools[1]?.description).toBe("MCP tool boom from echo");
  });

  it("routes a call by canonical id and joins the text parts", async () => {
    const { manager } = setup(
      [
        {
          alias: "echo",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { echo: echoServer },
    );
    const outcome = await manager.callTool("mcp.echo.echo", { text: "hi" });
    expect(outcome.text).toBe("echo:hi");
    expect(outcome.isError).toBe(false);
    expect(outcome.toolName).toBe("echo");
  });

  it("resolves a tool alias back to the server-side name when calling", async () => {
    const { manager } = setup(
      [
        {
          alias: "echo",
          transport: { kind: "stdio", command: "x" },
          toolAliases: { echo: "say" },
          resilience: FAST,
        },
      ],
      { echo: echoServer },
    );
    const tools = await manager.listTools("echo");
    expect(tools[0]?.canonicalId).toBe("mcp.echo.say");
    const outcome = await manager.callTool("mcp.echo.say", { text: "hi" });
    expect(outcome.toolName).toBe("echo");
    expect(outcome.text).toBe("echo:hi");
  });

  it("reports isError without throwing — the server answered, it just said no", async () => {
    const { manager } = setup(
      [
        {
          alias: "echo",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { echo: echoServer },
    );
    const outcome = await manager.callTool("mcp.echo.boom", {});
    expect(outcome.isError).toBe(true);
    expect(outcome.text).toBe("the tool refused");
  });

  it("isolates a failing server in connectAll and skips disabled ones", async () => {
    const { manager } = setup(
      [
        {
          alias: "good",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
        {
          alias: "bad",
          transport: { kind: "stdio", command: "x" },
          resilience: { ...FAST, maxConnectAttempts: 1 },
        },
        {
          alias: "off",
          transport: { kind: "stdio", command: "x" },
          enabled: false,
          resilience: FAST,
        },
      ],
      {
        good: echoServer,
        bad: () => {
          throw new Error("nope");
        },
        off: echoServer,
      },
    );
    const result = await manager.connectAll();
    expect(result.connected).toEqual(["good"]);
    expect(result.skipped).toEqual(["off"]);
    expect(result.failed.map((f) => f.alias)).toEqual(["bad"]);
    expect(result.failed[0]?.error.code).toBe("mcp_connect_failed");
    expect(manager.connectedAliases()).toEqual(["good"]);
  });

  it("fails an unknown alias rather than silently contributing nothing", async () => {
    const { manager } = setup(
      [
        {
          alias: "echo",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { echo: echoServer },
    );
    const failure = await manager
      .callTool("mcp.nope.tool", {})
      .then(() => null)
      .catch((err: unknown) => err);
    expect((failure as McpError).code).toBe("mcp_not_connected");
  });

  it("does not read a tool alias off the prototype chain", async () => {
    // `constructor` is a legal MCP tool name and a property every object has.
    // Read with a plain index, `toolAliases["constructor"]` hands a FUNCTION to
    // the alias grammar and the TypeError costs the server's whole tool set.
    const { manager } = setup(
      [
        {
          alias: "proto",
          transport: { kind: "stdio", command: "x" },
          toolAliases: { other: "renamed" },
          resilience: FAST,
        },
      ],
      {
        proto: () =>
          buildFakeServer("proto", [
            { name: "constructor", annotations: { readOnlyHint: true } },
          ]),
      },
    );
    const tools = await manager.listTools("proto");
    expect(tools.map((t) => t.canonicalId)).toEqual(["mcp.proto.constructor"]);
  });

  it("still finds the identity of a tool the server listed with padding", async () => {
    // The identity is normalized (trimmed); the listing is not. Matching the
    // two by NAME loses the tool — and with it, the whole batch.
    const { manager } = setup(
      [
        {
          alias: "pad",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { pad: () => buildFakeServer("pad", [{ name: " read " }]) },
    );
    const tools = await manager.listTools("pad");
    expect(tools).toHaveLength(1);
    expect(tools[0]?.canonicalId).toBe("mcp.pad.read");
  });

  it("dispose closes every session", async () => {
    const harness = new InMemoryHarness({
      echo: echoServer,
      other: echoServer,
    });
    const manager = new McpClientManager(
      {
        secrets: createSecretStore(),
        clock: createTestClock(),
        transportFactory: harness.factory,
      },
      [
        {
          alias: "echo",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
        {
          alias: "other",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
    );
    await manager.connectAll();
    expect(manager.connectedAliases()).toEqual(["echo", "other"]);
    await manager.dispose();
    expect(manager.connectedAliases()).toEqual([]);
    await harness.closeAll();
  });
});

describe("resilience", () => {
  it("times out a slow request with mcp_request_timeout, running it ONCE", async () => {
    // A timeout is an ambiguous delivery: the server may be halfway through the
    // write. Replaying it would run the tool again while the model is told the
    // call failed, so `mcp_request_timeout` is terminal by default however many
    // reconnect attempts are configured.
    const pending = deferred<{ content: { type: string; text: string }[] }>();
    let invocations = 0;
    const { manager, harness } = setup(
      [
        {
          alias: "slow",
          transport: { kind: "stdio", command: "x" },
          resilience: {
            ...FAST,
            requestTimeoutMs: 30,
            reconnectMaxAttempts: 2,
          },
        },
      ],
      {
        slow: () =>
          buildFakeServer("slow", [
            {
              name: "wait",
              handler: () => {
                invocations += 1;
                return pending.promise;
              },
            },
          ]),
      },
    );
    const failure = await manager
      .callTool("mcp.slow.wait", {})
      .then(() => null)
      .catch((err: unknown) => err);
    expect((failure as McpError).code).toBe("mcp_request_timeout");
    // Still advisory-retryable for the host and the model to act on...
    expect((failure as McpError).retryable).toBe(true);
    // ...but the session did not act on it itself.
    expect(invocations).toBe(1);
    expect(harness.connects("slow")).toBe(1);
    pending.resolve({ content: [{ type: "text", text: "late" }] });
  });

  it("replays a timeout only when retryTimeouts opts in, then reports mcp_reconnect_exhausted", async () => {
    // The opt-in for a server whose tools are known idempotent. It restores the
    // reconnect-and-re-issue loop — and with it the double execution, which is
    // exactly why it is off by default.
    const pending = deferred<{ content: { type: string; text: string }[] }>();
    let invocations = 0;
    const { manager, harness } = setup(
      [
        {
          alias: "slow",
          transport: { kind: "stdio", command: "x" },
          resilience: {
            ...FAST,
            requestTimeoutMs: 20,
            reconnectMaxAttempts: 1,
            retryTimeouts: true,
          },
        },
      ],
      {
        slow: () =>
          buildFakeServer("slow", [
            {
              name: "wait",
              handler: () => {
                invocations += 1;
                return pending.promise;
              },
            },
          ]),
      },
    );
    const failure = await manager
      .callTool("mcp.slow.wait", {})
      .then(() => null)
      .catch((err: unknown) => err);
    expect((failure as McpError).code).toBe("mcp_reconnect_exhausted");
    expect((failure as McpError).details?.["lastCode"]).toBe(
      "mcp_request_timeout",
    );
    // One initial connect plus exactly one reconnect — and one call each.
    expect(harness.connects("slow")).toBe(2);
    expect(invocations).toBe(2);
    pending.resolve({ content: [{ type: "text", text: "late" }] });
  });

  it("aborts an in-flight call when the run's signal fires", async () => {
    const pending = deferred<{ content: { type: string; text: string }[] }>();
    const { manager } = setup(
      [
        {
          alias: "slow",
          transport: { kind: "stdio", command: "x" },
          resilience: { ...FAST, requestTimeoutMs: 5_000 },
        },
      ],
      {
        slow: () =>
          buildFakeServer("slow", [
            { name: "wait", handler: () => pending.promise },
          ]),
      },
    );
    const controller = new AbortController();
    const call = manager.callTool(
      "mcp.slow.wait",
      {},
      {
        signal: controller.signal,
      },
    );
    setTimeout(() => controller.abort(), 10);
    const failure = await call.then(() => null).catch((err: unknown) => err);
    expect((failure as McpError).code).toBe("mcp_request_aborted");
    expect((failure as McpError).retryable).toBe(false);
    pending.resolve({ content: [{ type: "text", text: "late" }] });
  });

  it("opens a hard lockout after a failed connect cycle, then re-arms", async () => {
    const { manager, harness, clock } = setup(
      [
        {
          alias: "down",
          transport: { kind: "stdio", command: "x" },
          resilience: { ...FAST, maxConnectAttempts: 3, circuitOpenMs: 50 },
        },
      ],
      {
        down: () => {
          throw new Error("connection refused");
        },
      },
    );

    const first = await manager
      .connect("down")
      .then(() => null)
      .catch((e: unknown) => e);
    expect((first as McpError).code).toBe("mcp_connect_failed");
    expect(harness.connects("down")).toBe(3);

    // Locked out: the next connect must not touch the transport at all.
    const second = await manager
      .connect("down")
      .then(() => null)
      .catch((e: unknown) => e);
    expect((second as McpError).code).toBe("mcp_circuit_open");
    expect(harness.connects("down")).toBe(3);

    // No half-open probe: once the window elapses, a FULL fresh cycle runs.
    clock.advance(50);
    const third = await manager
      .connect("down")
      .then(() => null)
      .catch((e: unknown) => e);
    expect((third as McpError).code).toBe("mcp_connect_failed");
    expect(harness.connects("down")).toBe(6);
  });

  it("does not arm the lockout for a failure retrying cannot fix", async () => {
    const { manager, harness } = setup(
      [
        {
          alias: "gh",
          transport: {
            kind: "http",
            url: "https://example.test/mcp",
            headers: { Authorization: "Bearer ${GH_TOKEN}" },
          },
          secretRefs: { GH_TOKEN: "missing.ref" },
          resilience: FAST,
        },
      ],
      { gh: echoServer },
    );
    for (const _ of [0, 1]) {
      const failure = await manager
        .connect("gh")
        .then(() => null)
        .catch((e: unknown) => e);
      // Still the precise error on the second call, not mcp_circuit_open.
      expect((failure as McpError).code).toBe("mcp_secret_missing");
    }
    expect(harness.connects("gh")).toBe(0);
  });

  it("shares ONE reconnect between concurrent failing calls", async () => {
    const blocked = deferred<{ content: { type: string; text: string }[] }>();
    const { manager, harness } = setup(
      [
        {
          alias: "flap",
          transport: { kind: "stdio", command: "x" },
          resilience: {
            ...FAST,
            requestTimeoutMs: 2_000,
            reconnectMaxAttempts: 2,
          },
        },
      ],
      {
        flap: (attempt) =>
          buildFakeServer("flap", [
            {
              name: "work",
              handler: () =>
                // The first generation never answers; the replacement does.
                attempt === 1
                  ? blocked.promise
                  : {
                      content: [{ type: "text", text: `served-by-${attempt}` }],
                    },
            },
          ]),
      },
    );

    await manager.connect("flap");
    const a = manager.callTool("mcp.flap.work", {});
    const b = manager.callTool("mcp.flap.work", {});
    // Let both requests reach the (unresponsive) first server before it dies.
    await new Promise((resolve) => setTimeout(resolve, 5));
    await harness.dropCurrent("flap");

    expect(await Promise.all([a, b])).toMatchObject([
      { text: "served-by-2" },
      { text: "served-by-2" },
    ]);
    // Two concurrent failures, one reconnect: 1 initial + 1 = 2.
    expect(harness.connects("flap")).toBe(2);
    blocked.resolve({ content: [{ type: "text", text: "never read" }] });
  });

  it("closes a transport that finished connecting after dispose, never installs it", async () => {
    // The leak ADR 0004 says was fixed: `dispose()` returned while a connect
    // was still in flight, and the client (and, over stdio, its child process)
    // was installed on a session nobody would ever close again.
    const gate = deferred<null>();
    // A flag, not a count: closing one half of an `InMemoryTransport` pair
    // cascades back through the other, so the patched `close` sees itself twice.
    let closed = false;
    const servers: ReturnType<typeof buildFakeServer>[] = [];
    const transportFactory: McpTransportFactory = async () => {
      const server = echoServer();
      servers.push(server);
      const [clientTransport, serverTransport] =
        InMemoryTransport.createLinkedPair();
      await server.connect(serverTransport);
      // The connect parks HERE, where the transport exists and the session
      // cannot see it yet.
      await gate.promise;
      const close = clientTransport.close.bind(clientTransport);
      clientTransport.close = async () => {
        closed = true;
        await close();
      };
      return clientTransport;
    };
    const manager = new McpClientManager(
      {
        secrets: createSecretStore(),
        clock: createTestClock(),
        transportFactory,
      },
      [
        {
          alias: "slowopen",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
    );
    const connecting = manager
      .connect("slowopen")
      .then(() => null)
      .catch((err: unknown) => err);

    const disposing = manager.dispose();
    gate.resolve(null);
    const failure = await connecting;
    await disposing;

    expect(manager.isConnected("slowopen")).toBe(false);
    expect(closed).toBe(true);
    expect((failure as McpError | null)?.code).toBe("mcp_not_connected");
    for (const server of servers) await server.close();
  });

  it("does not resurrect a session closed DURING a reconnect", async () => {
    // The reconnect path re-opened what `close()` had just swept: `open()`
    // cleared the disposed flag, and `close()` waited only for a connect in
    // flight — not for a reconnect. `dispose()` returned, and a moment later a
    // fresh client (over stdio, a fresh child process) was installed on a
    // session nobody would ever close again.
    const gate = deferred<null>();
    const opened: { entered: boolean; closed: boolean }[] = [];
    const servers: ReturnType<typeof buildFakeServer>[] = [];
    let attempt = 0;
    const transportFactory: McpTransportFactory = async () => {
      attempt += 1;
      const server = buildFakeServer("flap", [
        // Never answers, so the call times out and (with `retryTimeouts`) the
        // session reconnects while still holding a live client to tear down.
        { name: "work", handler: () => new Promise(() => {}) },
      ]);
      servers.push(server);
      const [clientTransport, serverTransport] =
        InMemoryTransport.createLinkedPair();
      await server.connect(serverTransport);
      const record = { entered: false, closed: false };
      opened.push(record);
      const close = clientTransport.close.bind(clientTransport);
      const gated = attempt === 1;
      clientTransport.close = async () => {
        record.entered = true;
        // The reconnect's TEARDOWN parks here, so the `close()` below lands
        // between the reconnect's two halves — after the teardown, before the
        // open it exists to perform.
        if (gated) await gate.promise;
        await close();
        record.closed = true;
      };
      return clientTransport;
    };
    const manager = new McpClientManager(
      {
        secrets: createSecretStore(),
        clock: createTestClock(),
        transportFactory,
      },
      [
        {
          alias: "flap",
          transport: { kind: "stdio", command: "x" },
          resilience: {
            ...FAST,
            requestTimeoutMs: 40,
            retryTimeouts: true,
            reconnectMaxAttempts: 1,
          },
        },
      ],
    );

    await manager.connect("flap");
    const call = manager
      .callTool("mcp.flap.work", {})
      .then(() => null)
      .catch((err: unknown) => err);
    await until(() => opened[0]?.entered === true);

    const disposing = manager.dispose();
    gate.resolve(null);
    await disposing;
    await call;

    expect(manager.isConnected("flap")).toBe(false);
    // Nothing was opened after the close, and everything that was opened is
    // closed — the property `dispose()` is supposed to guarantee.
    expect(opened).toHaveLength(1);
    expect(opened.every((record) => record.closed)).toBe(true);
    for (const server of servers) await server.close();
  });

  it("gives up on a transport factory that never settles", async () => {
    // `withDeadline` races rather than awaits: a factory that ignores the
    // signal would otherwise hang forever, be cached as the shared `connecting`
    // promise, and take every later turn down with it — while the circuit
    // breaker waits for a failure that never arrives.
    const manager = new McpClientManager(
      {
        secrets: createSecretStore(),
        clock: createTestClock(),
        transportFactory: () => new Promise<Transport>(() => {}),
      },
      [
        {
          alias: "stuck",
          transport: { kind: "stdio", command: "x" },
          resilience: { ...FAST, connectTimeoutMs: 20, maxConnectAttempts: 1 },
        },
      ],
    );
    const failure = await manager
      .connect("stuck")
      .then(() => null)
      .catch((err: unknown) => err);
    expect((failure as McpError).code).toBe("mcp_connect_failed");
    expect(manager.isConnected("stuck")).toBe(false);
  });
});

/** Poll until `predicate` holds, so a test never guesses how long a phase takes. */
async function until(
  predicate: () => boolean,
  timeoutMs = 2_000,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("condition never became true");
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}
