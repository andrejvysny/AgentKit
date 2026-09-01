/**
 * The client's surface, checked against `REST_ROUTES` rather than against a
 * list somebody typed here.
 *
 * Two halves, and both are needed. The COMPILE-TIME half lives in the client
 * itself (`satisfies Record<RestOperation, unknown>`): a route added to the
 * contract breaks the build until a method exists for it. That is exhaustive
 * about NAMES and says nothing about behaviour — a `getChat` that POSTed to
 * `/v1/settings` would satisfy it. So the RUN-TIME half below drives every
 * method through a recording `fetch` and asserts the request it produced is the
 * method and path the route table declares, with the path parameters in the
 * right holes.
 */
import { describe, expect, test } from "bun:test";
import { REST_ROUTES, type RestOperation } from "@agentkit/contracts";
import { createAgentKitClient, type AgentKitClient } from "../src/index.js";

const BASE = "http://client.test";

const PATH_VALUES: Readonly<Record<string, string>> = {
  chatId: "c-1",
  messageId: "m-1",
  runId: "r-1",
  proposalId: "pr-1",
  providerId: "pv-1",
  allowanceId: "al-1",
  serverId: "sv-1",
};

interface Recorded {
  method: string;
  url: URL;
  headers: Headers;
  body?: string;
}

function recordingClient(): {
  client: AgentKitClient;
  calls: Recorded[];
} {
  const calls: Recorded[] = [];
  const client = createAgentKitClient({
    baseUrl: BASE,
    fetch: async (input, init) => {
      const url = new URL(input);
      calls.push({
        method: init?.method ?? "GET",
        url,
        headers: new Headers(init?.headers ?? {}),
        ...(typeof init?.body === "string" ? { body: init.body } : {}),
      });
      if (url.pathname.endsWith("/stream")) {
        // An SSE body that ends immediately: the iteration completes, the way
        // it does when the server has nothing left to send.
        return new Response("retry: 10\n\n", {
          status: 200,
          headers: { "content-type": "text/event-stream" },
        });
      }
      return new Response("{}", {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    },
  });
  return { client, calls };
}

/** How each operation is invoked. Keyed by operation ⇒ exhaustive at compile time. */
const DRIVERS = {
  createChat: (c) => c.createChat({}),
  listChats: (c) => c.listChats(),
  getChat: (c) => c.getChat({ chatId: "c-1" }),
  updateChat: (c) => c.updateChat({ chatId: "c-1" }, { title: "t" }),
  deleteChat: (c) => c.deleteChat({ chatId: "c-1" }),
  listMessages: (c) => c.listMessages({ chatId: "c-1" }),
  submitMessage: (c) => c.submitMessage({ chatId: "c-1" }, { content: "hi" }),
  regenerateMessage: (c) =>
    c.regenerateMessage({ chatId: "c-1", messageId: "m-1" }),
  forkChat: (c) => c.forkChat({ chatId: "c-1" }, { fromMessageId: "m-1" }),
  searchMessages: (c) => c.searchMessages({ q: "needle" }),
  activateBranch: (c) => c.activateBranch({ messageId: "m-1" }),
  listSiblings: (c) => c.listSiblings({ messageId: "m-1" }),
  getRun: (c) => c.getRun({ runId: "r-1" }),
  streamRun: async (c) => {
    for await (const _event of c.streamRun("r-1")) {
      // drained; the canned body carries no event
    }
  },
  cancelRun: (c) => c.cancelRun({ runId: "r-1" }),
  listToolEvents: (c) => c.listToolEvents({ chatId: "c-1" }),
  listProposals: (c) => c.listProposals({ chatId: "c-1" }),
  approveProposal: (c) => c.approveProposal({ proposalId: "pr-1" }),
  rejectProposal: (c) => c.rejectProposal({ proposalId: "pr-1" }),
  applyProposal: (c) =>
    c.applyProposal({ proposalId: "pr-1" }, { operationId: "op-1" }),
  listProviders: (c) => c.listProviders(),
  createProvider: (c) =>
    c.createProvider({
      label: "l",
      kind: "openai-compatible",
      baseUrl: "http://x",
      defaultModel: "m",
    }),
  updateProvider: (c) =>
    c.updateProvider({ providerId: "pv-1" }, { label: "l" }),
  deleteProvider: (c) => c.deleteProvider({ providerId: "pv-1" }),
  listModels: (c) => c.listModels({ providerId: "pv-1" }),
  refreshProviderModels: (c) => c.refreshProviderModels({ providerId: "pv-1" }),
  testProvider: (c) => c.testProvider({ providerId: "pv-1" }),
  getSettings: (c) => c.getSettings(),
  updateSettings: (c) => c.updateSettings({ allowRawToolData: true }),
  listAllowances: (c) => c.listAllowances({ chatId: "c-1" }),
  grantAllowance: (c) =>
    c.grantAllowance({
      chatId: "c-1",
      toolName: "t",
      proposalKind: "k",
      maxRisk: "low",
    }),
  revokeAllowance: (c) =>
    c.revokeAllowance({ allowanceId: "al-1", chatId: "c-1" }),
  listMcpServers: (c) => c.listMcpServers(),
  createMcpServer: (c) =>
    c.createMcpServer({
      alias: "gh",
      transport: { kind: "stdio", command: "gh-mcp" },
    }),
  updateMcpServer: (c) =>
    c.updateMcpServer({ serverId: "sv-1" }, { alias: "gh" }),
  deleteMcpServer: (c) => c.deleteMcpServer({ serverId: "sv-1" }),
  listTools: (c) => c.listTools(),
  getVersion: (c) => c.getVersion(),
} satisfies Record<RestOperation, (client: AgentKitClient) => Promise<unknown>>;

/** `/v1/chats/:chatId` → `/v1/chats/c-1`, with the values the drivers pass. */
function expectedPath(operation: RestOperation): string {
  return REST_ROUTES[operation].path.replace(
    /:([A-Za-z0-9_]+)/g,
    (_match, name: string) => {
      const value = PATH_VALUES[name];
      if (value === undefined) {
        throw new Error(`No placeholder for path parameter \`${name}\`.`);
      }
      return value;
    },
  );
}

describe("every route in the contract has a client method", () => {
  test("the operation sets are identical", () => {
    const contractOps = Object.keys(REST_ROUTES).sort();
    const { client } = recordingClient();
    const clientOps = contractOps.filter(
      (op) => typeof (client as Record<string, unknown>)[op] === "function",
    );
    expect(clientOps).toEqual(contractOps);
    // 38 operations, and the number is asserted so a route quietly disappearing
    // from the contract does not quietly shrink this test with it.
    expect(contractOps).toHaveLength(38);
  });

  for (const operation of Object.keys(REST_ROUTES) as RestOperation[]) {
    test(`${operation} → ${REST_ROUTES[operation].method} ${REST_ROUTES[operation].path}`, async () => {
      const { client, calls } = recordingClient();
      await DRIVERS[operation](client);
      expect(calls).toHaveLength(1);
      const call = calls[0]!;
      expect(call.method).toBe(REST_ROUTES[operation].method);
      expect(call.url.origin).toBe(BASE);
      expect(call.url.pathname).toBe(expectedPath(operation));
    });
  }
});

describe("headers and query parameters", () => {
  test("the two idempotent writes send an Idempotency-Key and nothing else does", async () => {
    for (const operation of Object.keys(REST_ROUTES) as RestOperation[]) {
      const { client, calls } = recordingClient();
      await DRIVERS[operation](client);
      const sent = calls[0]!.headers.get("idempotency-key");
      if (operation === "submitMessage" || operation === "regenerateMessage") {
        expect(sent).toBeString();
        expect(sent).not.toBe("");
      } else {
        expect(sent).toBeNull();
      }
    }
  });

  test("a minted key is a UUID, and a supplied one is sent verbatim", async () => {
    const { client, calls } = recordingClient();
    const minted = await client.submitMessage(
      { chatId: "c-1" },
      { content: "hi" },
    );
    expect(minted.idempotencyKey).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
    );
    expect(calls[0]!.headers.get("idempotency-key")).toBe(
      minted.idempotencyKey,
    );

    const replayed = await client.submitMessage(
      { chatId: "c-1" },
      { content: "hi" },
      { idempotencyKey: "my-own-key" },
    );
    expect(replayed.idempotencyKey).toBe("my-own-key");
    expect(calls[1]!.headers.get("idempotency-key")).toBe("my-own-key");
  });

  test("query parameters go where the route declares them", async () => {
    const { client, calls } = recordingClient();
    await client.listChats({ limit: 25, before: "2026-01-01" });
    await client.listMessages({ chatId: "c-1", limit: 10, cursor: "cur" });
    await client.searchMessages({ q: "a b", chatId: "c-1", limit: 5 });
    await client.listProposals({ chatId: "c-1", status: "pending" });
    await client.listAllowances({ chatId: "c-1" });
    await client.revokeAllowance({ allowanceId: "al-1", chatId: "c-1" });

    expect(calls[0]!.url.search).toBe("?limit=25&before=2026-01-01");
    expect(calls[1]!.url.search).toBe("?limit=10&cursor=cur");
    expect(calls[2]!.url.searchParams.get("q")).toBe("a b");
    expect(calls[3]!.url.searchParams.get("status")).toBe("pending");
    expect(calls[4]!.url.search).toBe("?chatId=c-1");
    expect(calls[5]!.url.search).toBe("?chatId=c-1");
  });

  test("an absent optional query parameter is omitted, not sent as undefined", async () => {
    const { client, calls } = recordingClient();
    await client.listMessages({ chatId: "c-1" });
    expect(calls[0]!.url.search).toBe("");
  });

  test("path parameters are percent-encoded", async () => {
    const { client, calls } = recordingClient();
    await client.getChat({ chatId: "a/b c" });
    expect(calls[0]!.url.pathname).toBe("/v1/chats/a%2Fb%20c");
  });

  test("a trailing slash on the base URL does not double up", async () => {
    const calls: string[] = [];
    const client = createAgentKitClient({
      baseUrl: "http://client.test/api/",
      fetch: async (input) => {
        calls.push(String(input));
        return new Response("{}", { status: 200 });
      },
    });
    await client.getVersion();
    expect(calls[0]).toBe("http://client.test/api/v1/version");
    expect(client.baseUrl).toBe("http://client.test/api");
  });

  test("per-call headers merge over the client-wide ones", async () => {
    const calls: Headers[] = [];
    const client = createAgentKitClient({
      baseUrl: BASE,
      headers: async () => ({ authorization: "Bearer base", "x-app": "one" }),
      fetch: async (_input, init) => {
        calls.push(new Headers(init?.headers ?? {}));
        return new Response("{}", { status: 200 });
      },
    });
    await client.getVersion({ headers: { "x-app": "two", "x-extra": "yes" } });
    expect(calls[0]!.get("authorization")).toBe("Bearer base");
    expect(calls[0]!.get("x-app")).toBe("two");
    expect(calls[0]!.get("x-extra")).toBe("yes");
  });

  test("a caller cannot displace the Idempotency-Key with a stray header", async () => {
    const { client, calls } = recordingClient();
    const submitted = await client.submitMessage(
      { chatId: "c-1" },
      { content: "hi" },
      { headers: { "idempotency-key": "not-this-one" } },
    );
    expect(calls[0]!.headers.get("idempotency-key")).toBe(
      submitted.idempotencyKey,
    );
    expect(submitted.idempotencyKey).not.toBe("not-this-one");
  });

  test("bodies are JSON, and a body-less route sends none", async () => {
    const { client, calls } = recordingClient();
    await client.createChat({ title: "t" });
    await client.getChat({ chatId: "c-1" });
    expect(calls[0]!.body).toBe('{"title":"t"}');
    expect(calls[0]!.headers.get("content-type")).toBe("application/json");
    expect(calls[1]!.body).toBeUndefined();
    expect(calls[1]!.headers.get("content-type")).toBeNull();
  });

  test("the stream route asks for text/event-stream, everything else for JSON", async () => {
    const { client, calls } = recordingClient();
    for await (const _event of client.streamRun("r-1")) {
      // drained
    }
    await client.getRun({ runId: "r-1" });
    expect(calls[0]!.headers.get("accept")).toBe("text/event-stream");
    expect(calls[1]!.headers.get("accept")).toBe("application/json");
  });
});
