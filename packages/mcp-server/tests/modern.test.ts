import { expect, test } from "bun:test";
import { Client as OldClient } from "mcp-sdk-v1/client/index.js";
import { StreamableHTTPClientTransport as OldTransport } from "mcp-sdk-v1/client/streamableHttp.js";
import {
  createMcpServerHandler,
  type McpSessionScope,
  type McpToolSource,
} from "../src/index.js";
import {
  authHeaders,
  connectClient,
  readRpcResponse,
  serveHandler,
  TEST_TOKEN,
} from "./helpers.js";

function source(seen: McpSessionScope[]): McpToolSource {
  return {
    catalog: {
      async listTools(scope) {
        seen.push(scope!);
        return [
          {
            namespace: "test",
            definition: {
              name: "echo",
              description: "echo",
              version: "1",
              effect: "read",
              capability: "echo",
              inputSchema: { type: "object" },
            },
          },
        ];
      },
    },
    async execute(_name, args, scope) {
      seen.push(scope!);
      return {
        ok: true,
        status: "ok",
        warnings: [],
        truncated: false,
        data: args,
      };
    },
  };
}
test("modern HTTP has fresh authenticated application actor per request and cleans each exactly once", async () => {
  const seen: McpSessionScope[] = [];
  const closed: string[] = [];
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    sessionScope: (headers) => ({
      principal: "verified",
      chatId: headers.get("x-chat") ?? "default",
      actorId: "forged",
    }),
    tools: {
      ...source(seen),
      closeSession: (scope) => {
        closed.push(scope.actorId!);
      },
    },
  });
  const served = serveHandler(handler);
  const { client, transport } = await connectClient(
    served.url,
    authHeaders({ "x-chat": "chat-a" }),
    "auto",
  );
  try {
    expect(transport.sessionId).toBeUndefined();
    await client.listTools();
    await client.callTool({ name: "echo", arguments: { exact: true } });
    await Bun.sleep(5);
    const actors = new Set(seen.map((scope) => scope.actorId));
    expect(actors.size).toBe(2);
    expect(actors.has("forged")).toBe(false);
    expect(
      seen.every(
        (scope) => scope.principal === "verified" && scope.chatId === "chat-a",
      ),
    ).toBe(true);
    expect(
      [...actors].every(
        (actor) =>
          closed.filter((closedActor) => actor === closedActor).length === 1,
      ),
    ).toBe(true);
    expect(seen.every((scope) => scope.signal?.aborted)).toBe(true);
  } finally {
    await client.close();
    await handler.dispose();
    await served.stop();
  }
});

test("official SDK1 HTTP client interoperates with sessionful compatibility adapter", async () => {
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    tools: source([]),
  });
  const served = serveHandler(handler);
  const client = new OldClient({ name: "old", version: "1" });
  const transport = new OldTransport(served.url, {
    requestInit: { headers: authHeaders() },
  });
  try {
    await client.connect(transport);
    expect((await client.listTools()).tools).toHaveLength(1);
    expect(
      (await client.callTool({ name: "echo", arguments: { old: true } }))
        .isError,
    ).toBeUndefined();
    expect(transport.sessionId).toBeString();
  } finally {
    await client.close();
    await handler.dispose();
    await served.stop();
  }
});

for (const revision of [
  "2025-11-25",
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
  "2024-10-07",
]) {
  test(`legacy ${revision} initializes and lists over real HTTP`, async () => {
    const handler = createMcpServerHandler({
      auth: { bearerToken: TEST_TOKEN },
      tools: source([]),
    });
    const served = serveHandler(handler);
    const headers = {
      ...authHeaders(),
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
    };
    try {
      const initialized = await fetch(served.url, {
        method: "POST",
        headers,
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 1,
          method: "initialize",
          params: {
            protocolVersion: revision,
            capabilities: {},
            clientInfo: { name: "legacy", version: "1" },
          },
        }),
      });
      const sessionId = initialized.headers.get("mcp-session-id");
      expect(sessionId).toBeString();
      const result = await readRpcResponse(initialized);
      expect(result.result).toMatchObject({ protocolVersion: revision });
      const response = await fetch(served.url, {
        method: "POST",
        headers: {
          ...headers,
          "mcp-session-id": sessionId!,
          "mcp-protocol-version": revision,
        },
        body: JSON.stringify({ jsonrpc: "2.0", id: 2, method: "tools/list" }),
      });
      expect((await readRpcResponse(response)).result).toMatchObject({
        tools: [{ name: "echo" }],
      });
    } finally {
      await handler.dispose();
      await served.stop();
    }
  });
}

test("modern per-principal admission stays bounded across concurrent request actors", async () => {
  let started!: () => void;
  let release!: () => void;
  const entered = new Promise<void>((resolve) => {
    started = resolve;
  });
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let calls = 0;
  const tools = source([]);
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    maxSessions: 1,
    tools: {
      ...tools,
      async execute(name, args, scope) {
        calls++;
        started();
        await gate;
        return tools.execute(name, args, scope);
      },
    },
  });
  const served = serveHandler(handler);
  const { client } = await connectClient(served.url, authHeaders(), "auto");
  try {
    const first = client.callTool({ name: "echo", arguments: {} });
    await entered;
    await expect(
      client.callTool({ name: "echo", arguments: {} }),
    ).rejects.toMatchObject({ data: { status: 503 } });
    expect(calls).toBe(1);
    release();
    await first;
    await client.callTool({ name: "echo", arguments: {} });
    expect(calls).toBe(2);
  } finally {
    release();
    await client.close();
    await handler.dispose();
    await served.stop();
  }
});

test("disconnect cannot bypass principal concurrency while noncooperative tool still runs", async () => {
  let started!: () => void;
  let release!: () => void;
  const entered = new Promise<void>((resolve) => {
    started = resolve;
  });
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const tools = source([]);
  let calls = 0;
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    maxConcurrentCallsPerSession: 1,
    tools: {
      ...tools,
      async execute(name, args, scope) {
        calls++;
        if (calls === 1) {
          started();
          await gate;
        }
        return tools.execute(name, args, scope);
      },
    },
  });
  const served = serveHandler(handler);
  const { client } = await connectClient(served.url, authHeaders(), "auto");
  const controller = new AbortController();
  try {
    const first = client
      .callTool({ name: "echo", arguments: {} }, { signal: controller.signal })
      .catch(() => undefined);
    await entered;
    controller.abort();
    await first;
    await Bun.sleep(5);
    const second = client.callTool({ name: "echo", arguments: {} });
    await Bun.sleep(5);
    expect(calls).toBe(1);
    release();
    await second;
    expect(calls).toBe(2);
  } finally {
    release();
    await client.close();
    await handler.dispose();
    await served.stop();
  }
});

for (const authorization of [
  `bearer ${TEST_TOKEN}`,
  `BEARER  ${TEST_TOKEN}`,
  `Bearer\t${TEST_TOKEN}`,
]) {
  test(`equivalent bearer form ${authorization.split(TEST_TOKEN)[0]} shares modern admission`, async () => {
    let started!: () => void;
    let release!: () => void;
    const entered = new Promise<void>((resolve) => {
      started = resolve;
    });
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const tools = source([]);
    let calls = 0;
    const handler = createMcpServerHandler({
      auth: { bearerToken: TEST_TOKEN },
      maxSessions: 1,
      tools: {
        ...tools,
        async execute(name, args, scope) {
          calls++;
          started();
          await gate;
          return tools.execute(name, args, scope);
        },
      },
    });
    const served = serveHandler(handler);
    const first = await connectClient(served.url, authHeaders(), "auto");
    const second = await connectClient(served.url, { authorization }, "auto");
    try {
      const running = first.client.callTool({ name: "echo", arguments: {} });
      await entered;
      const competing = second.client
        .callTool({ name: "echo", arguments: {} })
        .catch((error: unknown) => error);
      const refused = await Promise.race([
        competing,
        Bun.sleep(500).then(() => undefined),
      ]);
      expect(refused).toMatchObject({ data: { status: 503 } });
      expect(calls).toBe(1);
      release();
      await running;
    } finally {
      release();
      await first.client.close();
      await second.client.close();
      await handler.dispose();
      await served.stop();
    }
  });
}

test("equivalent bearer forms share modern tool concurrency without sharing actors", async () => {
  let started!: () => void;
  let arrived!: () => void;
  let release!: () => void;
  const entered = new Promise<void>((resolve) => {
    started = resolve;
  });
  const competingArrival = new Promise<void>((resolve) => {
    arrived = resolve;
  });
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const tools = source([]);
  let calls = 0;
  const actors: string[] = [];
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    maxConcurrentCallsPerSession: 1,
    tools: {
      ...tools,
      async execute(name, args, scope) {
        actors.push(scope!.actorId!);
        calls++;
        if (calls === 1) {
          started();
          await gate;
        }
        return tools.execute(name, args, scope);
      },
    },
  });
  const variant = `bEaReR\t  ${TEST_TOKEN}`;
  const served = serveHandler({
    ...handler,
    async fetch(request) {
      if (
        request.headers.get("authorization") === variant &&
        request.method === "POST"
      ) {
        const body = (await request.clone().json()) as { method?: string };
        if (body.method === "tools/call") arrived();
      }
      return handler.fetch(request);
    },
  });
  const first = await connectClient(served.url, authHeaders(), "auto");
  const second = await connectClient(
    served.url,
    { authorization: variant },
    "auto",
  );
  try {
    const running = first.client.callTool({ name: "echo", arguments: {} });
    await entered;
    const queued = second.client.callTool({ name: "echo", arguments: {} });
    await competingArrival;
    await Bun.sleep(10);
    expect(calls).toBe(1);
    release();
    await Promise.all([running, queued]);
    expect(calls).toBe(2);
    expect(new Set(actors).size).toBe(2);
  } finally {
    release();
    await first.client.close();
    await second.client.close();
    await handler.dispose();
    await served.stop();
  }
});
