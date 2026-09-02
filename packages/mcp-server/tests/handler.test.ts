import { afterEach, describe, expect, it } from "bun:test";
import type { AiTool } from "@agentkit/core";
import { defaultClock, defaultIds, type ToolGuard } from "@agentkit/host";
import {
  createMcpServerHandler,
  createStagedToolSource,
  type McpServerHandler,
  type McpServerHandlerOptions,
  type McpToolSource,
} from "../src/index.js";
import {
  authHeaders,
  CHAT_HEADER,
  connectClient,
  createTestClock,
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

/** A raw `initialize` POST body — no SDK client, so the test owns the headers. */
const INIT_BODY = JSON.stringify({
  jsonrpc: "2.0",
  id: 1,
  method: "initialize",
  params: {
    protocolVersion: "2025-06-18",
    capabilities: {},
    clientInfo: { name: "raw", version: "1.0.0" },
  },
});

/** The raw `initialize` POST, answered however the handler chooses. */
function initSessionRaw(
  handler: McpServerHandler,
  headers: Record<string, string> = authHeaders(),
): Promise<Response> {
  return handler.fetch(
    new Request("http://localhost/mcp", {
      method: "POST",
      headers: {
        host: "localhost",
        accept: "application/json, text/event-stream",
        "content-type": "application/json",
        ...headers,
      },
      body: INIT_BODY,
    }),
  );
}

/** Open a session over raw fetch and return the id the server minted. */
async function initSession(
  handler: McpServerHandler,
  headers: Record<string, string> = authHeaders(),
): Promise<string> {
  const response = await initSessionRaw(handler, headers);
  expect(response.status).toBe(200);
  const sessionId = response.headers.get("mcp-session-id");
  if (sessionId === null) throw new Error("initialize returned no session id");
  return sessionId;
}

/** One `tools/list` on an existing session, as a raw request. */
function listToolsRaw(
  handler: McpServerHandler,
  sessionId: string,
  headers: Record<string, string> = authHeaders(),
): Promise<Response> {
  return handler.fetch(
    new Request("http://localhost/mcp", {
      method: "POST",
      headers: {
        host: "localhost",
        accept: "application/json, text/event-stream",
        "content-type": "application/json",
        "mcp-session-id": sessionId,
        ...headers,
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list" }),
    }),
  );
}

describe("session binding", () => {
  /** Two tokens that are both valid — the point is that they are different principals. */
  const TOKEN_A = "principal-a-token-000";
  const TOKEN_B = "principal-b-token-111";
  const twoPrincipals = {
    auth: {
      verify: (header: string | null) =>
        header === `Bearer ${TOKEN_A}` || header === `Bearer ${TOKEN_B}`,
    },
  };
  const bearer = (token: string) => ({ authorization: `Bearer ${token}` });

  it("lets the principal that opened the session keep using it", async () => {
    const handler = build(twoPrincipals);
    const sessionId = await initSession(handler, bearer(TOKEN_A));
    const response = await listToolsRaw(handler, sessionId, bearer(TOKEN_A));
    expect(response.status).toBe(200);
    const rpc = await readRpcResponse(response);
    expect(
      (rpc["result"] as { tools: unknown[] }).tools.length,
    ).toBeGreaterThan(0);
  });

  it("answers a different principal on a leaked session id with 404", async () => {
    const handler = build(twoPrincipals);
    const sessionId = await initSession(handler, bearer(TOKEN_A));

    // B authenticated fine — it just is not whose session this is. The refusal
    // is the unknown-id one, so it does not confirm the id exists.
    const stolen = await listToolsRaw(handler, sessionId, bearer(TOKEN_B));
    expect(stolen.status).toBe(404);
    const invented = await listToolsRaw(
      handler,
      "not-a-session",
      bearer(TOKEN_B),
    );
    expect(invented.status).toBe(404);
    expect(await stolen.text()).toBe(await invented.text());
  });

  it("refuses a DELETE from another principal and leaves the session alive", async () => {
    const handler = build(twoPrincipals);
    const sessionId = await initSession(handler, bearer(TOKEN_A));

    const deleted = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "DELETE",
        headers: {
          host: "localhost",
          "mcp-session-id": sessionId,
          ...bearer(TOKEN_B),
        },
      }),
    );
    expect(deleted.status).toBe(404);

    // The owner still has it: the refusal cost B nothing and A nothing.
    expect(
      (await listToolsRaw(handler, sessionId, bearer(TOKEN_A))).status,
    ).toBe(200);
  });

  it("binds an unauthenticated-header session to the absent header", async () => {
    // A `verify` that accepts everything, including no header at all: the
    // fingerprint of the empty string is a fingerprint like any other.
    const handler = build({ auth: { verify: () => true } });
    const sessionId = await initSession(handler, {});
    expect((await listToolsRaw(handler, sessionId, {})).status).toBe(200);
    // ...and a caller who now presents one is a different principal.
    expect(
      (await listToolsRaw(handler, sessionId, bearer(TOKEN_A))).status,
    ).toBe(404);
  });
});

describe("session limits", () => {
  it("evicts the oldest idle session at maxSessions and closes its transport", async () => {
    const clock = createTestClock();
    const handler = build({ maxSessions: 2, clock });

    const first = await initSession(handler);
    // A standalone SSE stream on the victim: the eviction has to CLOSE the
    // transport, not just forget the map entry, and an ended stream is the
    // only observable difference between the two.
    const stream = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "GET",
        headers: {
          ...authHeaders(),
          host: "localhost",
          accept: "text/event-stream",
          "mcp-session-id": first,
        },
      }),
    );
    expect(stream.status).toBe(200);
    if (stream.body === null) throw new Error("no SSE body");
    const reader = stream.body.getReader();

    clock.advance(1_000);
    const second = await initSession(handler);
    clock.advance(1_000);
    const third = await initSession(handler);

    expect((await listToolsRaw(handler, first)).status).toBe(404);
    expect((await listToolsRaw(handler, second)).status).toBe(200);
    expect((await listToolsRaw(handler, third)).status).toBe(200);

    const streamEnded = await Promise.race([
      reader.read().then((chunk) => chunk.done),
      new Promise<boolean>((resolve) => {
        setTimeout(() => resolve(false), 500);
      }),
    ]);
    expect(streamEnded).toBe(true);
    await reader.cancel();
  });

  it("reaps a session idle past sessionIdleTtlMs on the next request", async () => {
    const clock = createTestClock();
    const handler = build({ maxSessions: 2, sessionIdleTtlMs: 60_000, clock });

    const first = await initSession(handler);
    const second = await initSession(handler);
    const warm = await listToolsRaw(handler, first);
    expect(warm.status).toBe(200);
    // Drained, because the ANSWER is written after `fetch` returns: until it
    // is, the session has a request in flight and is deliberately not reapable.
    await readRpcResponse(warm);

    clock.advance(60_001);
    // Lazily: this very request is what sweeps them.
    expect((await listToolsRaw(handler, first)).status).toBe(404);
    expect((await listToolsRaw(handler, second)).status).toBe(404);

    // And they no longer occupy a slot — two fresh sessions fit under the cap
    // of two and neither evicts the other.
    const third = await initSession(handler);
    const fourth = await initSession(handler);
    expect((await listToolsRaw(handler, third)).status).toBe(200);
    expect((await listToolsRaw(handler, fourth)).status).toBe(200);
  });
});

/** One raw POST of an arbitrary JSON-RPC payload on an existing session. */
function postRaw(
  handler: McpServerHandler,
  sessionId: string,
  payload: unknown,
  headers: Record<string, string> = authHeaders(),
): Promise<Response> {
  return handler.fetch(
    new Request("http://localhost/mcp", {
      method: "POST",
      headers: {
        host: "localhost",
        accept: "application/json, text/event-stream",
        "content-type": "application/json",
        "mcp-session-id": sessionId,
        ...headers,
      },
      body: JSON.stringify(payload),
    }),
  );
}

/** A promise a test resolves by hand, for "this call is still running" scenarios. */
function deferred(): { promise: Promise<void>; resolve(): void } {
  let resolve!: () => void;
  const promise = new Promise<void>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

/**
 * Let the transport's message dispatch run.
 *
 * `fetch` resolves as soon as the SSE stream exists — the handlers it will
 * write into have not started yet — so every assertion about what a call is
 * DOING has to wait for a macrotask first.
 */
function settle(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 5);
  });
}

/** A tool that parks inside `execute` until the test lets it go. */
function blockingTool(gate: Promise<void>, seen: { peak: number }): AiTool {
  let running = 0;
  return {
    definition: {
      name: "demo_block",
      version: "1.0.0",
      effect: "read",
      capability: "demo.block",
      description: "Blocks until released.",
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      running += 1;
      seen.peak = Math.max(seen.peak, running);
      await gate;
      running -= 1;
      return {
        ok: true,
        summary: "released",
        data: { released: true },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

function sourceOf(
  tools: AiTool[],
  guards?: readonly ToolGuard[],
): McpToolSource {
  return createStagedToolSource({
    contributors: [{ namespace: "demo", contribute: async () => tools }],
    clock: defaultClock,
    ids: defaultIds,
    ...(guards === undefined ? {} : { guards }),
  });
}

describe("request limits", () => {
  const oversized = (bytes: number): string =>
    JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: "2025-06-18",
        capabilities: {},
        clientInfo: { name: "x".repeat(bytes), version: "1.0.0" },
      },
    });

  it("refuses a body over maxRequestBytes with 413", async () => {
    const handler = build({ maxRequestBytes: 1024 });
    const response = await handler.fetch(
      new Request("http://localhost/mcp", {
        method: "POST",
        headers: {
          ...authHeaders(),
          host: "localhost",
          accept: "application/json, text/event-stream",
          "content-type": "application/json",
        },
        body: oversized(4096),
      }),
    );
    expect(response.status).toBe(413);
    // Nothing was created for it: the cap is checked before a session exists.
    expect(response.headers.get("mcp-session-id")).toBeNull();
  });

  it("refuses an oversized CHUNKED body, which declares no length", async () => {
    const handler = build({ maxRequestBytes: 1024 });
    const chunk = new TextEncoder().encode("x".repeat(512));
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        for (let i = 0; i < 8; i += 1) controller.enqueue(chunk);
        controller.close();
      },
    });
    const init: RequestInit & { duplex: "half" } = {
      method: "POST",
      headers: {
        ...authHeaders(),
        host: "localhost",
        accept: "application/json, text/event-stream",
        "content-type": "application/json",
      },
      body,
      duplex: "half",
    };
    const response = await handler.fetch(
      new Request("http://localhost/mcp", init),
    );
    expect(response.status).toBe(413);
  });

  it("refuses a batch longer than maxBatchSize without dispatching any of it", async () => {
    const gate = deferred();
    const seen = { peak: 0 };
    const handler = build({
      maxBatchSize: 2,
      tools: sourceOf([blockingTool(gate.promise, seen)]),
    });
    const sessionId = await initSession(handler);
    const batch = Array.from({ length: 25 }, (_unused, index) => ({
      jsonrpc: "2.0",
      id: index + 10,
      method: "tools/call",
      params: { name: "demo_block", arguments: {} },
    }));

    const response = await postRaw(handler, sessionId, batch);
    expect(response.status).toBe(400);
    const rpc = await readRpcResponse(response);
    expect((rpc["error"] as { code: number }).code).toBe(-32600);

    // The refusal is the whole point: not one of the 25 reached a tool.
    await settle();
    expect(seen.peak).toBe(0);
    gate.resolve();
  });

  it("runs at most maxConcurrentCallsPerSession calls of one batch at a time", async () => {
    const gate = deferred();
    const seen = { peak: 0 };
    const handler = build({
      maxConcurrentCallsPerSession: 2,
      maxBatchSize: 6,
      tools: sourceOf([blockingTool(gate.promise, seen)]),
    });
    const sessionId = await initSession(handler);
    const batch = Array.from({ length: 6 }, (_unused, index) => ({
      jsonrpc: "2.0",
      id: index + 10,
      method: "tools/call",
      params: { name: "demo_block", arguments: {} },
    }));

    const response = await postRaw(handler, sessionId, batch);
    await settle();
    expect(seen.peak).toBe(2);

    // Released, the queue drains and every message still gets its answer.
    gate.resolve();
    const frames = (await response.text())
      .split("\n")
      .filter((line) => line.startsWith("data:"));
    expect(frames).toHaveLength(6);
  });
});

describe("per-principal session capacity", () => {
  const TOKEN_A = "principal-a-token-000";
  const TOKEN_B = "principal-b-token-111";
  const twoPrincipals = {
    auth: {
      verify: (header: string | null) =>
        header === `Bearer ${TOKEN_A}` || header === `Bearer ${TOKEN_B}`,
    },
  };
  const bearer = (token: string) => ({ authorization: `Bearer ${token}` });

  it("lets a principal evict only its OWN oldest session", async () => {
    const clock = createTestClock();
    const handler = build({ ...twoPrincipals, maxSessions: 1, clock });

    const a = await initSession(handler, bearer(TOKEN_A));
    clock.advance(1_000);
    const b1 = await initSession(handler, bearer(TOKEN_B));
    clock.advance(1_000);
    // B is at ITS cap, so this closes B's own oldest — and nothing of A's,
    // however long A has been idle.
    const b2 = await initSession(handler, bearer(TOKEN_B));

    expect((await listToolsRaw(handler, a, bearer(TOKEN_A))).status).toBe(200);
    expect((await listToolsRaw(handler, b1, bearer(TOKEN_B))).status).toBe(404);
    expect((await listToolsRaw(handler, b2, bearer(TOKEN_B))).status).toBe(200);
  });
});

describe("in-flight calls", () => {
  it("does not reap a session whose tool call is still running", async () => {
    const clock = createTestClock();
    const gate = deferred();
    const seen = { peak: 0 };
    const handler = build({
      sessionIdleTtlMs: 60_000,
      clock,
      tools: sourceOf([blockingTool(gate.promise, seen)]),
    });
    const sessionId = await initSession(handler);
    const call = await postRaw(handler, sessionId, {
      jsonrpc: "2.0",
      id: 7,
      method: "tools/call",
      params: { name: "demo_block", arguments: {} },
    });
    await settle();
    expect(seen.peak).toBe(1);

    // The session has now been idle past its TTL by the clock — but the answer
    // to a call it is still running has to go somewhere.
    clock.advance(60_001);
    expect((await listToolsRaw(handler, sessionId)).status).toBe(200);

    gate.resolve();
    const rpc = await readRpcResponse(call);
    expect(rpc["result"]).toBeDefined();
  });

  it("refuses a new session with 503 when every session at the cap is busy", async () => {
    const clock = createTestClock();
    const gate = deferred();
    const seen = { peak: 0 };
    const handler = build({
      maxSessions: 1,
      clock,
      tools: sourceOf([blockingTool(gate.promise, seen)]),
    });
    const sessionId = await initSession(handler);
    const call = await postRaw(handler, sessionId, {
      jsonrpc: "2.0",
      id: 9,
      method: "tools/call",
      params: { name: "demo_block", arguments: {} },
    });
    await settle();
    expect(seen.peak).toBe(1);

    // The only session under the cap is in a tool call, so nothing is
    // evictable. Going over the cap here is what let a caller hold unbounded
    // sessions simply by keeping each one busy.
    const refused = await initSessionRaw(handler);
    expect(refused.status).toBe(503);
    expect(refused.headers.get("retry-after")).toBe("5");

    // And the live session is untouched: its answer still arrives.
    gate.resolve();
    const rpc = await readRpcResponse(call);
    expect(rpc["result"]).toBeDefined();
    expect((await listToolsRaw(handler, sessionId)).status).toBe(200);
  });

  it("reaps a session pinned by a call that outlived maxCallMs", async () => {
    const clock = createTestClock();
    const gate = deferred();
    const seen = { peak: 0 };
    const handler = build({
      sessionIdleTtlMs: 60_000,
      maxCallMs: 30_000,
      clock,
      // NOT the staged source's deadline: a host-supplied McpToolSource obeys
      // none, which is exactly the case the reap backstop exists for.
      tools: sourceOf([blockingTool(gate.promise, seen)]),
    });
    const sessionId = await initSession(handler);
    await postRaw(handler, sessionId, {
      jsonrpc: "2.0",
      id: 11,
      method: "tools/call",
      params: { name: "demo_block", arguments: {} },
    });
    await settle();
    expect(seen.peak).toBe(1);

    // Still inside maxCallMs + idle TTL: a slow call is not a stuck one.
    clock.advance(60_000);
    expect((await listToolsRaw(handler, sessionId)).status).toBe(200);

    clock.advance(30_001);
    expect((await listToolsRaw(handler, sessionId)).status).toBe(404);
    gate.resolve();
  });
});

describe("session principal", () => {
  const PRINCIPAL_HEADER = "x-agentkit-principal";

  function principalTool(seen: { tool?: unknown }): AiTool {
    return {
      definition: {
        name: "demo_whoami",
        version: "1.0.0",
        effect: "read",
        capability: "demo.whoami",
        description: "Reports the calling principal.",
        inputSchema: { type: "object", properties: {} },
      },
      async execute(ctx) {
        seen.tool = ctx.metadata?.["principal"];
        return {
          ok: true,
          summary: "ok",
          data: { principal: ctx.metadata?.["principal"] },
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    };
  }

  it("threads the session's principal to the tool context and the guards", async () => {
    const seen: { tool?: unknown; guard?: string | undefined } = {};
    const guard: ToolGuard = {
      canExecute: (ctx) => {
        seen.guard = ctx.principal;
        return { allowed: true };
      },
    };
    const handler = build({
      tools: sourceOf([principalTool(seen)], [guard]),
      sessionScope: (headers) => {
        const principal = headers.get(PRINCIPAL_HEADER);
        return principal === null ? {} : { principal };
      },
    });
    const served = serve(handler);
    const { client } = await connectClient(
      served.url,
      authHeaders({ [PRINCIPAL_HEADER]: "alice" }),
    );
    const result = await client.callTool({
      name: "demo_whoami",
      arguments: {},
    });
    await client.close();

    expect(seen.tool).toBe("alice");
    expect(seen.guard).toBe("alice");
    const content = result.content as { type: string; text?: unknown }[];
    expect(JSON.parse(textBlock(content, 1))).toEqual({ principal: "alice" });
  });
});
