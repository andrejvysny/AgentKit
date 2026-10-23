import { afterEach, expect, test } from "bun:test";
import {
  ListToolsRequestSchema,
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
  ListPromptsRequestSchema,
  GetPromptRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import {
  McpClientManager,
  createMcpToolSetContributor,
  assertSupportedMcpProtocol,
} from "../src/index.js";
import {
  InMemoryHarness,
  buildFakeServer,
  createSecretStore,
  toolContext,
} from "./helpers.js";
const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const close of cleanups.splice(0)) await close();
});
const fast = {
  connectBackoffBaseMs: 0,
  connectBackoffMaxMs: 0,
  requestTimeoutMs: 100,
};
function manager(
  server: ReturnType<typeof buildFakeServer>,
  discovery?: { maxPages?: number; maxItems?: number; maxBytes?: number },
) {
  const harness = new InMemoryHarness({ test: () => server });
  const manager = new McpClientManager(
    { secrets: createSecretStore(), transportFactory: harness.factory },
    [
      {
        alias: "test",
        transport: { kind: "stdio", command: "test" },
        resilience: fast,
        discovery,
      },
    ],
  );
  cleanups.push(() => manager.dispose());
  return { manager, harness };
}
test("lost acknowledgement never replays a mutation, even when timeout replay is enabled", async () => {
  let writes = 0;
  const harness = new InMemoryHarness({
    test: () =>
      buildFakeServer("test", [
        {
          name: "create",
          handler: async () => {
            writes++;
            await harness.dropCurrent("test");
            return { content: [{ type: "text", text: "created" }] };
          },
        },
      ]),
  });
  const client = new McpClientManager(
    { secrets: createSecretStore(), transportFactory: harness.factory },
    [
      {
        alias: "test",
        transport: { kind: "stdio", command: "test" },
        resilience: { ...fast, retryTimeouts: true },
      },
    ],
  );
  cleanups.push(() => client.dispose());
  await expect(client.callTool("mcp.test.create", {})).rejects.toMatchObject({
    code: "mcp_outcome_uncertain",
    retryable: false,
  });
  expect(writes).toBe(1);
  expect(harness.connects("test")).toBe(1);
});
test("discovers all pages and preserves case-sensitive remote names through call routing", async () => {
  const server = buildFakeServer("test", [
    { name: "getUser" },
    { name: "DATA_EXPORT_v2" },
  ]);
  const cursors: unknown[] = [];
  server.setRequestHandler(ListToolsRequestSchema, async (request) => {
    cursors.push(request.params?.cursor);
    return request.params?.cursor
      ? { tools: [{ name: "DATA_EXPORT_v2", inputSchema: { type: "object" } }] }
      : {
          tools: [{ name: "getUser", inputSchema: { type: "object" } }],
          nextCursor: "next",
        };
  });
  const { manager: client } = manager(server);
  expect((await client.listTools("test")).map((tool) => tool.toolName)).toEqual(
    ["getUser", "DATA_EXPORT_v2"],
  );
  expect(cursors).toEqual([undefined, "next"]);
  expect(
    (await client.callTool("mcp.test.getUser", { name: "test" })).toolName,
  ).toBe("getUser");
});
test("repeated cursors and catalog bounds fail closed", async () => {
  const server = buildFakeServer("test", []);
  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: [],
    nextCursor: "loop",
  }));
  const { manager: client } = manager(server);
  await expect(client.listTools("test")).rejects.toMatchObject({
    code: "mcp_catalog_limit",
  });
  const { manager: bounded } = manager(
    buildFakeServer("test", [{ name: "first" }, { name: "second" }]),
    { maxItems: 1 },
  );
  await expect(bounded.listTools("test")).rejects.toMatchObject({
    code: "mcp_catalog_limit",
  });
});
test("default bridge policy does not trust remote readOnlyHint", async () => {
  let calls = 0;
  const { manager: client } = manager(
    buildFakeServer("test", [
      {
        name: "read",
        annotations: { readOnlyHint: true },
        handler: async () => {
          calls++;
          return { content: [] };
        },
      },
    ]),
  );
  const tools = await createMcpToolSetContributor(client).contribute({
    bindings: [],
    limits: toolContext().limits,
  });
  const result = await tools[0]!.execute(toolContext(), {});
  expect(result.ok).toBe(false);
  expect(result.modelData).toMatchObject({
    errorCode: "mcp_invocation_refused",
  });
  expect(calls).toBe(0);
});
test("cancelled discovery never opens an unselected or cancelled connection", async () => {
  const { manager: client, harness } = manager(buildFakeServer("test", []));
  const controller = new AbortController();
  controller.abort();
  await expect(
    createMcpToolSetContributor(client).contribute({
      bindings: [],
      limits: toolContext().limits,
      signal: controller.signal,
    }),
  ).rejects.toBeDefined();
  expect(harness.connects("test")).toBe(0);
});
test("protocol matrix rejects unsupported revisions explicitly", () => {
  expect(() => assertSupportedMcpProtocol("2025-11-25")).not.toThrow();
  expect(() => assertSupportedMcpProtocol("2026-07-28")).toThrow(
    "Unsupported MCP protocol",
  );
});
test("remote context catalogs paginate and preserve roles and untrusted provenance", async () => {
  const server = buildFakeServer("test", []);
  server.registerCapabilities({ resources: {}, prompts: {} });
  server.setRequestHandler(ListResourcesRequestSchema, async (request) =>
    request.params?.cursor
      ? { resources: [{ uri: "remote://second", name: "second" }] }
      : {
          resources: [{ uri: "remote://first", name: "first" }],
          nextCursor: "next",
        },
  );
  server.setRequestHandler(ReadResourceRequestSchema, async (request) => ({
    contents: [{ uri: request.params.uri, text: "source material" }],
  }));
  server.setRequestHandler(ListPromptsRequestSchema, async () => ({
    prompts: [{ name: "review" }],
  }));
  server.setRequestHandler(GetPromptRequestSchema, async () => ({
    messages: [
      { role: "user", content: { type: "text", text: "Review" } },
      { role: "assistant", content: { type: "text", text: "Ready" } },
    ],
  }));
  const { manager: client } = manager(server);
  expect((await client.listResources("test")).map((item) => item.uri)).toEqual([
    "remote://first",
    "remote://second",
  ]);
  expect(await client.readResource("test", "remote://first")).toMatchObject({
    provenance: { serverAlias: "test", trusted: false },
  });
  expect((await client.listPrompts("test"))[0]?.name).toBe("review");
  const prompt = await client.getPrompt("test", "review");
  expect(prompt.messages.map((message) => message.role)).toEqual([
    "user",
    "assistant",
  ]);
  expect(prompt.provenance.trusted).toBe(false);
});
test("an explicit read authorizer can refuse and its private failures remain private", async () => {
  let calls = 0;
  const { manager: client } = manager(
    buildFakeServer("test", [
      {
        name: "read",
        handler: async () => {
          calls++;
          return { content: [] };
        },
      },
    ]),
  );
  const [tool] = await createMcpToolSetContributor(client, {
    classify: () => "read",
    authorize: async () => {
      throw new Error("private-credential");
    },
  }).contribute({ bindings: [], limits: toolContext().limits });
  const result = await tool!.execute(toolContext(), {});
  expect(result.ok).toBe(false);
  expect(JSON.stringify(result)).not.toContain("private-credential");
  expect(calls).toBe(0);
});
