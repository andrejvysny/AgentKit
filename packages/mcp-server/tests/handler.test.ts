import { afterEach, describe, expect, it } from "bun:test";
import { defaultClock, defaultIds } from "@agentkit/host";
import {
  createMcpServerHandler,
  createStagedToolSource,
  type McpServerHandler,
  type McpServerHandlerOptions,
} from "../src/index.js";
import {
  authHeaders,
  CHAT_HEADER,
  connectClient,
  demoContributor,
  echoTool,
  readRpcResponse,
  serveHandler,
  TEST_TOKEN,
  textBlock,
  type ServedHandler,
} from "./helpers.js";

/** Everything a test opened, torn down in `afterEach` even when it failed. */
const opened: { handlers: McpServerHandler[]; served: ServedHandler[] } = {
  handlers: [],
  served: [],
};

afterEach(async () => {
  for (const served of opened.served.splice(0)) await served.stop();
  for (const handler of opened.handlers.splice(0)) await handler.dispose();
});

function build(
  overrides: Partial<McpServerHandlerOptions> = {},
): McpServerHandler {
  const handler = createMcpServerHandler({
    tools: createStagedToolSource({
      contributors: [demoContributor()],
      clock: defaultClock,
      ids: defaultIds,
    }),
    auth: { bearerToken: TEST_TOKEN },
    sessionScope: (headers) => {
      const chatId = headers.get(CHAT_HEADER);
      return chatId === null ? {} : { chatId };
    },
    ...overrides,
  });
  opened.handlers.push(handler);
  return handler;
}

function serve(handler: McpServerHandler): ServedHandler {
  const served = serveHandler(handler);
  opened.served.push(served);
  return served;
}

describe("tools/list over the official SDK client", () => {
  it("projects AiToolDefinitions verbatim, without the namespace", async () => {
    const served = serve(build());
    const { client } = await connectClient(served.url, authHeaders());
    const { tools } = await client.listTools();
    await client.close();

    const echo = tools.find((tool) => tool.name === "demo_echo");
    if (echo === undefined) throw new Error("demo_echo was not listed");
    const definition = echoTool().definition;
    expect(echo.description).toBe(definition.description);
    expect(echo.inputSchema).toEqual(
      definition.inputSchema as unknown as typeof echo.inputSchema,
    );
    // The contributor namespace is attribution, not part of the callable name.
    expect(tools.every((tool) => !tool.name.startsWith("demo__"))).toBe(true);
  });

  it("hides write tools by default and refuses to call them", async () => {
    const served = serve(build());
    const { client } = await connectClient(served.url, authHeaders());
    const { tools } = await client.listTools();
    expect(tools.map((tool) => tool.name)).not.toContain("demo_write");

    // Hiding is not the whole policy: the call path filters too, so a client
    // that learned the name elsewhere still cannot reach it.
    await expect(
      client.callTool({ name: "demo_write", arguments: {} }),
    ).rejects.toThrow(/Unknown tool: demo_write/);
    await client.close();
  });

  it("lists and calls write tools when writesEnabled is true", async () => {
    const served = serve(build({ writesEnabled: true }));
    const { client } = await connectClient(served.url, authHeaders());
    const { tools } = await client.listTools();
    expect(tools.map((tool) => tool.name)).toContain("demo_write");

    const result = await client.callTool({ name: "demo_write", arguments: {} });
    expect(result.isError).toBeUndefined();
    await client.close();
  });

  it("answers an unknown tool with a protocol error, not a crash", async () => {
    const served = serve(build());
    const { client } = await connectClient(served.url, authHeaders());
    await expect(
      client.callTool({ name: "does_not_exist", arguments: {} }),
    ).rejects.toThrow(/Unknown tool/);
    // The session survives it.
    expect((await client.listTools()).tools.length).toBeGreaterThan(0);
    await client.close();
  });
});

describe("tools/call over the official SDK client", () => {
  it("returns the summary block then the envelope payload as JSON", async () => {
    const served = serve(build());
    const { client } = await connectClient(served.url, authHeaders());
    const result = await client.callTool({
      name: "demo_echo",
      arguments: { text: "hi" },
    });
    await client.close();

    const content = result.content as { type: string; text?: unknown }[];
    expect(textBlock(content, 0)).toBe("echoed 2 char(s)");
    // `modelData`, not the fuller UI `data` — the same thing a chat turn sees.
    expect(JSON.parse(textBlock(content, 1))).toEqual({ echo: "hi" });
    expect(result.isError).toBeUndefined();
  });

  it("flags a failed tool as isError with structured data", async () => {
    const served = serve(build());
    const { client } = await connectClient(served.url, authHeaders());
    const result = await client.callTool({
      name: "demo_fail",
      arguments: {},
    });
    await client.close();

    expect(result.isError).toBe(true);
    const content = result.content as { type: string; text?: unknown }[];
    expect(JSON.parse(textBlock(content, 1))).toMatchObject({
      errorCode: "demo_broken",
    });
  });

  it("reports bad arguments as a validation failure", async () => {
    const served = serve(build());
    const { client } = await connectClient(served.url, authHeaders());
    const result = await client.callTool({
      name: "demo_echo",
      arguments: { text: 42 },
    });
    await client.close();

    expect(result.isError).toBe(true);
    const content = result.content as { type: string; text?: unknown }[];
    expect(JSON.parse(textBlock(content, 1))).toMatchObject({
      errorCode: "schema_invalid",
      phase: "validation",
    });
  });
});

describe("bearer auth", () => {
  const post = (handler: McpServerHandler, headers: Record<string, string>) =>
    handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: { host: "localhost", ...headers },
        body: "{}",
      }),
    );

  it("refuses a missing Authorization header with a bodyless 401", async () => {
    const handler = build();
    const response = await post(handler, {});
    expect(response.status).toBe(401);
    expect(await response.text()).toBe("");
    expect(response.headers.get("www-authenticate")).toContain("Bearer");
  });

  it("refuses a wrong token", async () => {
    const response = await post(build(), { authorization: "Bearer nope" });
    expect(response.status).toBe(401);
  });

  it("refuses a wrong token of the SAME length", async () => {
    const sameLength = `${TEST_TOKEN.slice(0, -1)}X`;
    expect(sameLength).toHaveLength(TEST_TOKEN.length);
    const response = await post(build(), {
      authorization: `Bearer ${sameLength}`,
    });
    expect(response.status).toBe(401);
  });

  it("checks auth before the session lookup — an unknown session is still 401", async () => {
    const response = await post(build(), { "mcp-session-id": "made-up" });
    expect(response.status).toBe(401);
  });
});

describe("DNS-rebinding guard", () => {
  const initBody = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "raw", version: "1.0.0" },
    },
  });

  const init = (handler: McpServerHandler, headers: Record<string, string>) =>
    handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders(),
          accept: "application/json, text/event-stream",
          "content-type": "application/json",
          ...headers,
        },
        body: initBody,
      }),
    );

  it("refuses a Host outside the allow list with a bodyless 403", async () => {
    const response = await init(build(), { host: "evil.com" });
    expect(response.status).toBe(403);
    expect(await response.text()).toBe("");
  });

  it("allows loopback on any port by default", async () => {
    const response = await init(build(), { host: "127.0.0.1:54321" });
    expect(response.status).toBe(200);
  });

  it("refuses an Origin outside the allow list once one is configured", async () => {
    const handler = build({ allowedOrigins: ["http://localhost:5173"] });
    expect(
      (await init(handler, { host: "localhost", origin: "http://evil.com" }))
        .status,
    ).toBe(403);
    expect(
      (
        await init(handler, {
          host: "localhost",
          origin: "http://localhost:5173",
        })
      ).status,
    ).toBe(200);
  });

  it("honours a custom allowedHosts list, ports included", async () => {
    const handler = build({ allowedHosts: ["desktop.internal:9000"] });
    expect((await init(handler, { host: "localhost" })).status).toBe(403);
    expect(
      (await init(handler, { host: "desktop.internal:9001" })).status,
    ).toBe(403);
    expect(
      (await init(handler, { host: "desktop.internal:9000" })).status,
    ).toBe(200);
  });
});

describe("sessions and scope", () => {
  it("keys a session per client and gives each its own scope's tools", async () => {
    const served = serve(build());
    const a = await connectClient(
      served.url,
      authHeaders({ [CHAT_HEADER]: "chat-a" }),
    );
    const b = await connectClient(
      served.url,
      authHeaders({ [CHAT_HEADER]: "chat-b" }),
    );

    expect(a.transport.sessionId).toBeDefined();
    expect(b.transport.sessionId).toBeDefined();
    expect(a.transport.sessionId).not.toBe(b.transport.sessionId);

    const namesA = (await a.client.listTools()).tools.map((t) => t.name);
    const namesB = (await b.client.listTools()).tools.map((t) => t.name);
    expect(namesA).toContain("demo_only_chat-a");
    expect(namesA).not.toContain("demo_only_chat-b");
    expect(namesB).toContain("demo_only_chat-b");
    expect(namesB).not.toContain("demo_only_chat-a");

    await a.client.close();
    await b.client.close();
  });

  it("pins the scope at init — a later header cannot move the session", async () => {
    const handler = build();
    const served = serve(handler);
    const a = await connectClient(
      served.url,
      authHeaders({ [CHAT_HEADER]: "chat-a" }),
    );
    const sessionId = a.transport.sessionId;
    expect(sessionId).toBeDefined();

    // The same session, re-asserting a different scope on the wire. The handler
    // resolved `sessionScope` once, at initialize, and never looks again.
    const response = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders({ [CHAT_HEADER]: "chat-b" }),
          host: "localhost",
          accept: "application/json, text/event-stream",
          "content-type": "application/json",
          "mcp-session-id": String(sessionId),
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 99, method: "tools/list" }),
      }),
    );
    const rpc = await readRpcResponse(response);
    const names = (rpc["result"] as { tools: { name: string }[] }).tools.map(
      (tool) => tool.name,
    );
    expect(names).toContain("demo_only_chat-a");
    expect(names).not.toContain("demo_only_chat-b");

    await a.client.close();
  });

  it("refuses an unknown session id, and a non-initialize POST without one", async () => {
    const handler = build();
    const unknown = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders(),
          host: "localhost",
          "content-type": "application/json",
          "mcp-session-id": "not-a-session",
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
      }),
    );
    expect(unknown.status).toBe(404);

    const noSession = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders(),
          host: "localhost",
          "content-type": "application/json",
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
      }),
    );
    expect(noSession.status).toBe(400);

    const getWithoutSession = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "GET",
        headers: { ...authHeaders(), host: "localhost" },
      }),
    );
    expect(getWithoutSession.status).toBe(400);
  });

  it("forgets a session the client deletes", async () => {
    const handler = build();
    const served = serve(handler);
    const { client, transport } = await connectClient(
      served.url,
      authHeaders(),
    );
    const sessionId = String(transport.sessionId);
    // The explicit MCP session teardown: DELETE /mcp with the session header.
    // (`Client.close()` only drops the client's own streams — it does not tell
    // the server, which is exactly why the server must handle DELETE.)
    await transport.terminateSession();
    await client.close();

    const after = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders(),
          host: "localhost",
          accept: "application/json, text/event-stream",
          "content-type": "application/json",
          "mcp-session-id": sessionId,
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
      }),
    );
    expect(after.status).toBe(404);
  });
});

describe("dispose", () => {
  it("closes live sessions and stops answering", async () => {
    const handler = build();
    const served = serve(handler);
    const { client, transport } = await connectClient(
      served.url,
      authHeaders(),
    );
    const sessionId = String(transport.sessionId);
    expect((await client.listTools()).tools.length).toBeGreaterThan(0);

    await handler.dispose();

    const after = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders(),
          host: "localhost",
          "content-type": "application/json",
          "mcp-session-id": sessionId,
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
      }),
    );
    expect(after.status).toBe(503);

    // Idempotent: a second signal must not throw.
    await handler.dispose();
  });
});
