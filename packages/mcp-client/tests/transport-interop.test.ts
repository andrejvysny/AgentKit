import { afterEach, expect, test } from "bun:test";
import {
  CLIENT_CAPABILITIES_META_KEY,
  PROTOCOL_VERSION_META_KEY,
  createMcpHandler,
  inputRequired,
  Server,
} from "@modelcontextprotocol/server";
import { Server as LegacyServer } from "mcp-sdk-v1/server/index.js";
import { WebStandardStreamableHTTPServerTransport as LegacyTransport } from "mcp-sdk-v1/server/webStandardStreamableHttp.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "mcp-sdk-v1/types.js";
import { McpClientManager, type McpClientManagerDeps } from "../src/index.js";
import { createSecretStore } from "./helpers.js";

const cleanups: (() => Promise<unknown>)[] = [];
afterEach(async () => {
  for (const close of cleanups.splice(0).reverse()) await close();
});
const definition = {
  name: "echo",
  inputSchema: { type: "object" as const },
  annotations: { readOnlyHint: true },
};
function manager(url: URL, deps: Partial<McpClientManagerDeps> = {}) {
  const manager = new McpClientManager(
    { secrets: createSecretStore(), ...deps },
    [
      {
        alias: "peer",
        transport: { kind: "http", url: url.toString() },
        resilience: { maxConnectAttempts: 1, requestTimeoutMs: 2000 },
      },
    ],
  );
  cleanups.push(() => manager.dispose());
  return manager;
}
function serve(fetch: (request: Request) => Promise<Response>) {
  const server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch });
  cleanups.push(() => server.stop(true));
  return new URL(`http://127.0.0.1:${server.port}/mcp`);
}
function modernPeer(options: { ask?: boolean; lost?: boolean } = {}) {
  let writes = 0;
  const capabilities: unknown[] = [];
  const handler = createMcpHandler(
    () => {
      const server = new Server(
        { name: "modern", version: "1" },
        { capabilities: { tools: {} } },
      );
      server.setRequestHandler("tools/list", () => ({ tools: [definition] }));
      server.setRequestHandler("tools/call", (request, context) => {
        const envelope = context.mcpReq.envelope as
          | Readonly<Record<string, unknown>>
          | undefined;
        capabilities.push(envelope?.[CLIENT_CAPABILITIES_META_KEY]);
        if (options.ask && !context.mcpReq.inputResponses?.confirm) {
          return inputRequired({
            inputRequests: {
              confirm: inputRequired.elicit({
                message: "Confirm",
                requestedSchema: {
                  type: "object",
                  properties: { confirm: { type: "boolean" } },
                  required: ["confirm"],
                },
              }),
            },
            requestState: "opaque-state",
          });
        }
        if (options.ask) {
          expect(context.mcpReq.requestState<string>()).toBe("opaque-state");
          expect(context.mcpReq.inputResponses?.confirm).toEqual({
            action: "accept",
            content: { confirm: true },
          });
        }
        writes++;
        if (options.lost) return new Promise(() => {});
        return {
          content: [
            {
              type: "text",
              text: JSON.stringify({
                arguments: request.params.arguments,
                protocol: envelope?.[PROTOCOL_VERSION_META_KEY],
              }),
            },
          ],
        };
      });
      return server;
    },
    { legacy: "reject" },
  );
  cleanups.push(() => handler.close());
  return { handler, capabilities, writes: () => writes };
}

test("real HTTP negotiates modern envelopes; input_required executes mutation only after callback", async () => {
  const peer = modernPeer({ ask: true });
  let asked = 0;
  const client = manager(serve(peer.handler.fetch), {
    elicitation: async () => {
      asked++;
      expect(peer.writes()).toBe(0);
      return { action: "accept", content: { confirm: true } };
    },
  });
  expect((await client.listTools("peer"))[0]?.canonicalId).toBe(
    "mcp.peer.echo",
  );
  const result = await client.callTool("mcp.peer.echo", { exact: "value" });
  expect(result.text).toContain("2026-07-28");
  expect(result.text).toContain('"exact":"value"');
  expect(asked).toBe(1);
  expect(peer.writes()).toBe(1);
  expect(peer.capabilities[0]).toMatchObject({
    elicitation: { form: {}, url: {} },
  });
});

test("modern discovery auth wall preserves actionable authentication classification", async () => {
  const client = manager(
    serve(async () => new Response(null, { status: 401 })),
  );
  await expect(client.listTools("peer")).rejects.toMatchObject({
    code: "mcp_auth_required",
    retryable: false,
  });
});

test("real modern HTTP advertises no elicitation without host callback", async () => {
  const peer = modernPeer();
  const client = manager(serve(peer.handler.fetch));
  await client.callTool("mcp.peer.echo", {});
  expect(peer.capabilities[0]).toEqual({});
});

test("real modern HTTP cancellation aborts pending input without dispatching mutation", async () => {
  const peer = modernPeer({ ask: true });
  const controller = new AbortController();
  let observed = false;
  const client = manager(serve(peer.handler.fetch), {
    elicitation: async (_params, context) => {
      controller.abort();
      await new Promise<void>((resolve) => {
        if (context.signal.aborted) resolve();
        else
          context.signal.addEventListener("abort", () => resolve(), {
            once: true,
          });
      });
      observed = context.signal.aborted;
      return { action: "cancel" };
    },
  });
  await expect(
    client.callTool("mcp.peer.echo", {}, { signal: controller.signal }),
  ).rejects.toMatchObject({ code: "mcp_outcome_uncertain", retryable: false });
  await Bun.sleep(5);
  expect(observed).toBe(true);
  expect(peer.writes()).toBe(0);
});

test("modern HTTP lost mutation acknowledgement is uncertain, never retried through fallback", async () => {
  const peer = modernPeer();
  const methods: string[] = [];
  const url = serve(async (request) => {
    const message = (await request.clone().json()) as { method: string };
    methods.push(message.method);
    const response = await peer.handler.fetch(request);
    if (message.method !== "tools/call") return response;
    await response.text();
    return new Response("acknowledgement lost", { status: 502 });
  });
  const client = manager(url);
  await expect(
    client.callTool("mcp.peer.echo", { mutate: true }),
  ).rejects.toMatchObject({ code: "mcp_outcome_uncertain", retryable: false });
  expect(peer.writes()).toBe(1);
  expect(methods.filter((method) => method === "tools/call")).toHaveLength(1);
  expect(methods).not.toContain("initialize");
});

test("real HTTP falls back to official SDK1 peer before tool dispatch", async () => {
  let calls = 0;
  const url = serve(async (request) => {
    const transport = new LegacyTransport({
      sessionIdGenerator: undefined,
      enableJsonResponse: true,
    });
    const server = new LegacyServer(
      { name: "old", version: "1" },
      { capabilities: { tools: {} } },
    );
    server.setRequestHandler(ListToolsRequestSchema, () => ({
      tools: [definition],
    }));
    server.setRequestHandler(CallToolRequestSchema, (request) => {
      calls++;
      return {
        content: [
          { type: "text", text: JSON.stringify(request.params.arguments) },
        ],
      };
    });
    await server.connect(transport);
    return transport.handleRequest(request);
  });
  const client = manager(url);
  expect(await client.listTools("peer")).toHaveLength(1);
  expect((await client.callTool("mcp.peer.echo", { old: true })).text).toBe(
    '{"old":true}',
  );
  expect(calls).toBe(1);
});

for (const runtime of ["node", "bun"]) {
  for (const era of ["modern", "legacy"]) {
    test(`real spawned ${runtime} stdio negotiates ${era} peer`, async () => {
      const client = new McpClientManager({ secrets: createSecretStore() }, [
        {
          alias: "peer",
          transport: {
            kind: "stdio",
            command: runtime,
            args: [
              new URL("./fixtures/mcp-peer.mjs", import.meta.url).pathname,
              era,
            ],
          },
          resilience: {
            maxConnectAttempts: 1,
            connectTimeoutMs: 3000,
            requestTimeoutMs: 2000,
          },
        },
      ]);
      cleanups.push(() => client.dispose());
      expect(await client.listTools("peer")).toHaveLength(1);
      const result = await client.callTool("mcp.peer.echo", { runtime, era });
      expect(result.text).toContain(runtime);
      if (era === "modern") expect(result.text).toContain("2026-07-28");
    });
  }
}
