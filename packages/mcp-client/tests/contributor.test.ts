import { afterEach, describe, expect, it } from "bun:test";
import { AiToolRegistry, resolveToolLimits } from "@agentkit/core";
import type { ToolContributionContext } from "@agentkit/host";
import {
  createMcpToolSetContributor,
  McpClientManager,
  McpError,
  type McpConnectAllResult,
  type McpServerConfig,
  type McpToolCallOutcome,
  type McpToolDescriptor,
  type McpToolSource,
} from "../src/index.js";
import {
  buildFakeServer,
  createSecretStore,
  createTestClock,
  deferred,
  InMemoryHarness,
  toolContext,
} from "./helpers.js";

const FAST = {
  requestTimeoutMs: 60,
  connectTimeoutMs: 60,
  connectBackoffBaseMs: 1,
  connectBackoffMaxMs: 4,
  circuitOpenMs: 50,
} as const;

const CTX: ToolContributionContext = {
  chatId: "chat-1",
  runId: "run-1",
  bindings: [],
  limits: resolveToolLimits({ preference: "small" }),
};

/**
 * A schema with keywords `AiJsonSchemaObject` cannot name and NO
 * `additionalProperties`. Both matter: the first proves the document is passed
 * through rather than re-derived, the second that nothing is injected.
 */
const RICH_SCHEMA = {
  type: "object",
  properties: {
    query: { type: "string", minLength: 1, pattern: "^[a-z]+$" },
    mode: { oneOf: [{ const: "fast" }, { const: "slow" }] },
  },
  required: ["query"],
} as const;

const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  while (cleanups.length > 0) await cleanups.pop()?.();
});

function searchServer(): ReturnType<typeof buildFakeServer> {
  return buildFakeServer("search-server", [
    {
      name: "search",
      description: "Search the index",
      inputSchema: RICH_SCHEMA as unknown as Record<string, unknown>,
      annotations: { readOnlyHint: true, destructiveHint: false },
      handler: (args) => ({
        content: [
          { type: "text", text: `hit: ${String(args?.["query"])}` },
          { type: "image", data: "AAA", mimeType: "image/png" },
        ],
        structuredContent: { count: 1 },
      }),
    },
    {
      name: "purge",
      handler: () => ({
        content: [{ type: "text", text: "refused: read-only mode" }],
        isError: true,
      }),
    },
  ]);
}

function setup(
  configs: McpServerConfig[],
  builders: Record<string, () => ReturnType<typeof buildFakeServer>>,
): McpClientManager {
  const harness = new InMemoryHarness(builders);
  const manager = new McpClientManager(
    {
      secrets: createSecretStore(),
      clock: createTestClock(),
      transportFactory: harness.factory,
    },
    configs,
  );
  cleanups.push(async () => {
    await manager.dispose();
    await harness.closeAll();
  });
  return manager;
}

describe("createMcpToolSetContributor", () => {
  it("projects MCP tools onto AiTools, schema verbatim", async () => {
    const manager = setup(
      [
        {
          alias: "idx",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { idx: searchServer },
    );
    const tools = await createMcpToolSetContributor(manager).contribute(CTX);
    const search = tools[0]?.definition;
    expect(search?.name).toBe("mcp__idx__search");
    // The canonical id survives on `capability` — it is the routing key.
    expect(search?.capability).toBe("mcp.idx.search");
    expect(search?.description).toBe("Search the index");
    expect(search?.effect).toBe("read");
    expect(search?.inputSchema).toEqual(RICH_SCHEMA as unknown as object);
    expect(Object.keys(search?.inputSchema ?? {})).not.toContain(
      "additionalProperties",
    );
    expect(tools[1]?.definition.effect).toBe("write");
  });

  it("contributes names the tool registry actually accepts", async () => {
    const manager = setup(
      [
        {
          alias: "idx",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { idx: searchServer },
    );
    const tools = await createMcpToolSetContributor(manager).contribute(CTX);
    const registry = new AiToolRegistry();
    // Registering is the real assertion: a dotted canonical id would be refused
    // here and the tool silently dropped during registry staging.
    for (const tool of tools) registry.register(tool);
    expect(registry.size()).toBe(2);
    expect(registry.validateInput("mcp__idx__search", { query: "ok" })).toEqual(
      [],
    );
    // The server's own constraints are enforced, because its schema was kept.
    expect(
      registry.validateInput("mcp__idx__search", { query: "NOPE" }).length,
    ).toBeGreaterThan(0);
  });

  it("executes a call and returns the joined text as modelData", async () => {
    const manager = setup(
      [
        {
          alias: "idx",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { idx: searchServer },
    );
    const [search] = await createMcpToolSetContributor(manager).contribute(CTX);
    const result = await search!.execute(toolContext(), { query: "abc" });
    expect(result.ok).toBe(true);
    expect(result.modelData).toEqual({
      text: "hit: abc",
      structured: { count: 1 },
    });
    // Non-text parts are kept for the UI and flagged, not silently dropped.
    expect((result.data as McpToolCallOutcome).content).toHaveLength(2);
    expect(result.warnings[0]).toContain("image");
  });

  it("turns a server-reported tool error into a structured failure result", async () => {
    const manager = setup(
      [
        {
          alias: "idx",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { idx: searchServer },
    );
    const tools = await createMcpToolSetContributor(manager).contribute(CTX);
    const purge = tools.find((t) => t.definition.name === "mcp__idx__purge");
    const result = await purge!.execute(toolContext(), {});
    expect(result.ok).toBe(false);
    expect(result.modelData).toEqual({
      errorCode: "mcp_tool_error",
      errorMessage: "refused: read-only mode",
      retryable: false,
    });
  });

  it("never throws a bridge failure into the run loop, and keeps the code", async () => {
    const pending = deferred<{ content: { type: string; text: string }[] }>();
    const manager = setup(
      [
        {
          alias: "slow",
          transport: { kind: "stdio", command: "x" },
          resilience: {
            ...FAST,
            requestTimeoutMs: 30,
            reconnectMaxAttempts: 0,
          },
        },
      ],
      {
        slow: () =>
          buildFakeServer("slow", [
            { name: "wait", handler: () => pending.promise },
          ]),
      },
    );
    const [wait] = await createMcpToolSetContributor(manager).contribute(CTX);
    const result = await wait!.execute(toolContext(), {});
    expect(result.ok).toBe(false);
    const data = result.modelData as { errorCode: string; retryable: boolean };
    expect(data.errorCode).toBe("mcp_request_timeout");
    expect(data.retryable).toBe(true);
    pending.resolve({ content: [{ type: "text", text: "late" }] });
  });

  it("contributes the reachable servers when another one is down", async () => {
    const manager = setup(
      [
        {
          alias: "idx",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
        {
          alias: "down",
          transport: { kind: "stdio", command: "x" },
          resilience: { ...FAST, maxConnectAttempts: 1 },
        },
      ],
      {
        idx: searchServer,
        down: () => {
          throw new Error("refused");
        },
      },
    );
    const warned: string[] = [];
    const tools = await createMcpToolSetContributor(manager).contribute({
      ...CTX,
      logger: {
        debug: () => {},
        info: () => {},
        warn: (message) => warned.push(message),
        error: () => {},
      },
    });
    expect(tools.map((t) => t.definition.capability)).toEqual([
      "mcp.idx.search",
      "mcp.idx.purge",
    ]);
    expect(warned).toContain("mcp server skipped for this run");
  });

  it("fails the whole contribution when two servers claim one canonical id", async () => {
    const descriptor = (serverAlias: string): McpToolDescriptor => ({
      canonicalId: "mcp.shared.search",
      registryName: "mcp__shared__search",
      serverAlias,
      toolName: "search",
      effectiveToolName: "search",
      description: "search",
      inputSchema: { type: "object" },
      effect: "read",
    });
    const source: McpToolSource = {
      async connectAll(): Promise<McpConnectAllResult> {
        return { connected: ["a", "b"], failed: [], skipped: [] };
      },
      connectedAliases: () => ["a", "b"],
      async listTools(alias) {
        return [descriptor(alias)];
      },
      async callTool() {
        throw new Error("unreachable");
      },
    };
    const failure = await createMcpToolSetContributor(source)
      .contribute(CTX)
      .then(() => null)
      .catch((err: unknown) => err);
    expect(failure).toBeInstanceOf(McpError);
    expect((failure as McpError).code).toBe("mcp_canonical_id_collision");
  });

  it("fails the contribution when two servers project onto one registry name", async () => {
    // The registry name, not the canonical id, is what `AiToolRegistry` keys on
    // — so two DISTINCT canonical ids landing on one registry name shadow each
    // other just as badly. A well-formed `McpToolSource` cannot produce this
    // (server aliases carry no `_` or `.`), which is exactly why the guard is
    // here: a host-supplied source that derives `registryName` its own way must
    // fail closed rather than contribute a set with a hidden tool in it.
    const descriptor = (
      serverAlias: string,
      canonicalId: string,
    ): McpToolDescriptor => ({
      canonicalId,
      registryName: "mcp__shared__files__read",
      serverAlias,
      toolName: "files.read",
      effectiveToolName: "files.read",
      description: "read",
      inputSchema: { type: "object" },
      effect: "read",
    });
    const byAlias: Record<string, string> = {
      a: "mcp.a.files.read",
      b: "mcp.b.files.read",
    };
    const source: McpToolSource = {
      async connectAll(): Promise<McpConnectAllResult> {
        return { connected: ["a", "b"], failed: [], skipped: [] };
      },
      connectedAliases: () => ["a", "b"],
      async listTools(alias) {
        return [descriptor(alias, byAlias[alias]!)];
      },
      async callTool() {
        throw new Error("unreachable");
      },
    };
    const failure = await createMcpToolSetContributor(source)
      .contribute(CTX)
      .then(() => null)
      .catch((err: unknown) => err);
    expect(failure).toBeInstanceOf(McpError);
    expect((failure as McpError).code).toBe("mcp_canonical_id_collision");
    expect((failure as McpError).message).toContain("mcp__shared__files__read");
  });

  it("opts out of unbound pruning rather than deleting other contributors' tools", () => {
    const manager = setup(
      [
        {
          alias: "idx",
          transport: { kind: "stdio", command: "x" },
          resilience: FAST,
        },
      ],
      { idx: searchServer },
    );
    expect(
      createMcpToolSetContributor(manager).unboundToolNames,
    ).toBeUndefined();
  });
});
