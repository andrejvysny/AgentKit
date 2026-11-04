/**
 * Pre-deletion parity: the behaviours the TS `packages/mcp-server` suite
 * pinned (groups 9–13 of the Phase-1 gate map) asserted against the native
 * binding's Rust-hosted MCP server. The TS lane is being deleted; these are
 * its SDK1 interop, protocol-freeze, actor-identity, admission, and 401
 * challenges-port assertions, ported verbatim in semantics to
 * `startMcpServer` (same status codes, headers, JSON-RPC codes, dispatch
 * counters as `packages/mcp-server/tests/{modern,inbound-actors}.test.ts`).
 *
 * Conventions copied from `mcp.test.ts`: real binding, real HTTP on
 * 127.0.0.1 ephemeral ports, bounded poll-on-signal waits, no guessed sleeps
 * beyond the fixed observation windows used by the TS originals.
 */
import { expect, test } from "bun:test";
import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
// The pinned legacy SDK under the workspace alias (`mcp-sdk-v1` =
// npm:@modelcontextprotocol/sdk@1.30.0) — the same specifier the
// node-smoke step and the mcp-client interop suite import.
import { Client as LegacyClient } from "mcp-sdk-v1/client/index.js";
import { StreamableHTTPClientTransport as LegacyTransport } from "mcp-sdk-v1/client/streamableHttp.js";
import { NativeStore } from "../src/index.js";
import type { NativeSessionService, ToolDefinitionJson } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

/** 32 bytes: the exact bearer floor the binding enforces. */
const TOKEN = "12345678901234567890123456789012";

const ECHO_READ: ToolDefinitionJson = {
  name: "echo",
  effect: "read",
  capability: "test.echo",
  description: "Echo a value",
  inputSchema: {
    type: "object",
    properties: { value: { type: "string" } },
    required: ["value"],
  },
};

const MARK_WRITE: ToolDefinitionJson = {
  name: "mark",
  effect: "write",
  capability: "test.mark",
  description: "Mark a value",
  inputSchema: {
    type: "object",
    properties: { note: { type: "string" } },
    required: ["note"],
  },
};

const HOLD_READ: ToolDefinitionJson = {
  name: "hold",
  effect: "read",
  capability: "test.hold",
  description: "Hold until released",
  inputSchema: { type: "object" },
};

async function withService(): Promise<{
  store: NativeStore;
  service: NativeSessionService;
}> {
  const store = await NativeStore.inMemory();
  const service = await store.createService();
  return { store, service };
}

/** Poll-and-backoff on an observable signal (same convention as
 * `mcp.test.ts` / `authority.test.ts`): bounded, event-driven reads only. */
async function waitFor<T>(
  read: () => Promise<T> | T,
  accept: (value: T) => boolean,
  timeoutMs = 10_000,
): Promise<T> {
  const started = Date.now();
  let value = await read();
  while (!accept(value) && Date.now() - started < timeoutMs) {
    await Bun.sleep(20);
    value = await read();
  }
  if (!accept(value)) throw new Error("bounded wait expired");
  return value;
}

function gate(): { promise: Promise<void>; release: () => void } {
  const { promise, resolve } = Promise.withResolvers<void>();
  return { promise, release: resolve };
}

function newMcpClient(
  url: string,
  token: string,
): [Client, StreamableHTTPClientTransport] {
  const client = new Client(
    { name: "native-mcp-compat", version: "1.0.0" },
    { capabilities: {} },
  );
  const transport = new StreamableHTTPClientTransport(new URL(url), {
    requestInit: { headers: { authorization: `Bearer ${token}` } },
  });
  return [client, transport];
}

/** The wire headers the TS legacy suite (`modern.test.ts`) presents. */
function rpcHeaders(token: string, extra?: Record<string, string>) {
  return {
    authorization: `Bearer ${token}`,
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
    ...extra,
  };
}

/** Read a JSON-RPC answer whether the server returned it as JSON or as one
 * SSE `data:` frame (both are legal streamable-HTTP responses). */
async function readRpc(response: Response): Promise<Record<string, unknown>> {
  const text = await response.text();
  const contentType = response.headers.get("content-type") ?? "";
  if (contentType.includes("text/event-stream")) {
    for (const line of text.split("\n")) {
      if (!line.startsWith("data:")) continue;
      const payload = line.slice(5).trim();
      if (payload === "") continue; // SSE keep-alive / ping frames.
      let value: Record<string, unknown>;
      try {
        value = JSON.parse(payload) as Record<string, unknown>;
      } catch {
        continue;
      }
      if (value.jsonrpc === "2.0" && ("result" in value || "error" in value))
        return value;
    }
    throw new Error(`no JSON-RPC frame in SSE body: ${text}`);
  }
  return JSON.parse(text) as Record<string, unknown>;
}

// ---------------------------------------------------------------------------
// Group 9 — official SDK1 (1.30.0) client against the native server: live
// connect / list / call over real HTTP (port of modern.test.ts
// "official SDK1 HTTP client interoperates with sessionful compatibility
// adapter").
// ---------------------------------------------------------------------------

nativeTest(
  "MCP server: the pinned SDK1 (1.30.0) HTTP client connects, lists, and calls",
  async () => {
    const { store, service } = await withService();
    let echoCalls = 0;
    try {
      await service.registerTool(ECHO_READ, async (input) => {
        echoCalls += 1;
        const value =
          typeof input === "object" &&
          input !== null &&
          "value" in input &&
          typeof input.value === "string"
            ? input.value
            : "";
        return { data: { echoed: value }, summary: `echo:${value}` };
      });
      await service.registerTool(MARK_WRITE, async () => {
        throw new Error(
          "write tools stay uncallable while writesEnabled is false",
        );
      });

      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      const client = new LegacyClient({ name: "sdk1-native", version: "1" });
      const transport = new LegacyTransport(new URL(info.url), {
        requestInit: { headers: { authorization: `Bearer ${TOKEN}` } },
      });
      try {
        await client.connect(transport);
        // Sessionful interop: the legacy transport negotiated a real session.
        expect(transport.sessionId).toBeString();
        const listed = await client.listTools();
        expect(listed.tools).toHaveLength(1);
        expect(listed.tools[0]?.name).toBe("echo");
        const called = (await client.callTool({
          name: "echo",
          arguments: { value: "legacy" },
        })) as { isError?: boolean; content?: unknown };
        expect(called.isError).toBeUndefined();
        expect(JSON.stringify(called.content)).toContain("echo:legacy");
        expect(echoCalls).toBe(1);
      } finally {
        await client.close().catch(() => undefined);
      }

      // A wrong token never reaches the handler through the legacy transport.
      const stranger = new LegacyClient({
        name: "sdk1-stranger",
        version: "1",
      });
      const strangerTransport = new LegacyTransport(new URL(info.url), {
        requestInit: {
          headers: { authorization: `Bearer ${"0".repeat(32)}` },
        },
      });
      await expect(stranger.connect(strangerTransport)).rejects.toThrow();

      await service.stopMcpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// Group 10 — protocol freeze over REAL HTTP: each of the five frozen
// revisions must initialize AND complete a tools/list on the established
// session (port of modern.test.ts's `legacy ${revision}` loop; the N4 Rust
// pins stay complementary — per-revision LIST results are asserted here).
// ---------------------------------------------------------------------------

for (const revision of [
  "2025-11-25",
  "2025-06-18",
  "2025-03-26",
  "2024-11-05",
  "2024-10-07",
]) {
  nativeTest(
    `MCP server: legacy ${revision} initializes and lists over real HTTP`,
    async () => {
      const { store, service } = await withService();
      try {
        await service.registerTool(ECHO_READ, async () => ({ data: "listed" }));
        const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
        const headers = rpcHeaders(TOKEN);
        try {
          const initialized = await fetch(`${info.url}/`, {
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
          expect(initialized.status).toBe(200);
          const sessionId = initialized.headers.get("mcp-session-id");
          expect(typeof sessionId).toBe("string");
          const result = await readRpc(initialized);
          expect(result.result).toMatchObject({ protocolVersion: revision });

          const listed = await fetch(`${info.url}/`, {
            method: "POST",
            headers: {
              ...headers,
              "mcp-session-id": sessionId as string,
              "mcp-protocol-version": revision,
            },
            body: JSON.stringify({
              jsonrpc: "2.0",
              id: 2,
              method: "tools/list",
            }),
          });
          expect(listed.status).toBe(200);
          const listedResult = await readRpc(listed);
          expect(listedResult.result).toMatchObject({
            tools: [{ name: "echo" }],
          });
        } finally {
          await service.stopMcpServer().catch(() => undefined);
        }
      } finally {
        await service.close();
        await store.close();
      }
    },
    15_000,
  );
}

// ---------------------------------------------------------------------------
// Group 11 — same-token clients are distinct actors: each session gets its
// own `mcp:<actor>` run identity (never the session id), and DELETE revokes
// ONLY the caller's in-flight callback (port of inbound-actors.test.ts
// "same-name clients have distinct actors … disconnect revokes only own
// grant", translated to the native lane where revocation = aborted callback
// signal, wired by `Backend::revoke_actor`).
// ---------------------------------------------------------------------------

nativeTest(
  "MCP server: same-token clients run as distinct actors; DELETE revokes only the caller's in-flight call",
  async () => {
    const { store, service } = await withService();
    const g = gate();
    interface Seen {
      runId: string;
      signal: AbortSignal | undefined;
      finished: boolean;
    }
    const seen: Seen[] = [];
    try {
      await service.registerTool(HOLD_READ, async (_input, request) => {
        const entry: Seen = {
          runId: request.runId,
          signal: request.signal,
          finished: false,
        };
        seen.push(entry);
        // Deliberately NON-cooperative: the callback ignores its signal,
        // exactly the shape the TS "noncooperative tool" tests use. The
        // revocation witness is the abort the ROUTER raises on the signal.
        await g.promise;
        entry.finished = true;
        return { data: "held" };
      });
      await service.registerTool(ECHO_READ, async () => ({
        data: "alive",
      }));

      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      const [a, aTransport] = newMcpClient(info.url, TOKEN);
      const [b, bTransport] = newMcpClient(info.url, TOKEN);
      await a.connect(aTransport);
      await b.connect(bTransport);
      expect(aTransport.sessionId).toBeString();
      expect(bTransport.sessionId).toBeString();
      expect(aTransport.sessionId).not.toBe(bTransport.sessionId);

      // Caller A's promise is deliberately NOT awaited after its session is
      // deleted: the server tears the session down under the in-flight POST,
      // so that response never returns (the revocation witness is the
      // aborted callback signal, asserted below). Detach so a rejection
      // after teardown stays handled.
      const heldA = a.callTool({ name: "hold", arguments: {} }).then(
        () => undefined,
        () => undefined,
      );
      void heldA;
      await waitFor(
        () => seen.length,
        (length) => length === 1,
      );
      const heldB = b.callTool({ name: "hold", arguments: {} });
      await waitFor(
        () => seen.length,
        (length) => length === 2,
      );

      const runIdA = seen[0]!.runId;
      const runIdB = seen[1]!.runId;
      // Host-owned synthetic run identity, one actor per session — and the
      // actor is NOT the session id (TS: `expect(aActor).not.toBe(sessionId)`).
      expect(runIdA.startsWith("mcp:")).toBe(true);
      expect(runIdB.startsWith("mcp:")).toBe(true);
      expect(runIdA).not.toBe(runIdB);
      expect(runIdA.slice(4)).not.toBe(aTransport.sessionId);
      expect(runIdB.slice(4)).not.toBe(bTransport.sessionId);

      // Caller A deletes its session; only A's in-flight callback is revoked.
      await aTransport.terminateSession();
      await waitFor(
        () => seen[0]!.signal?.aborted === true,
        (aborted) => aborted,
      );
      expect(seen[1]!.signal?.aborted ?? false).toBe(false);

      g.release();
      const calledB = await heldB;
      expect(JSON.stringify(calledB.content)).toContain("held");
      expect(seen[1]!.signal?.aborted ?? false).toBe(false);

      // The survivor is fully usable; the deleted session is gone forever.
      const survivor = await b.callTool({
        name: "echo",
        arguments: { value: "alive" },
      });
      expect(JSON.stringify(survivor.content)).toContain("alive");
      const orphan = await fetch(`${info.url}/`, {
        method: "POST",
        headers: rpcHeaders(TOKEN, {
          "mcp-session-id": aTransport.sessionId as string,
          "mcp-protocol-version": "2025-06-18",
        }),
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 3,
          method: "tools/list",
        }),
      });
      expect(orphan.status).toBe(404);

      await b.close().catch(() => undefined);
      await a.close().catch(() => undefined);
      await service.stopMcpServer();
    } finally {
      g.release();
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// Group 12 — admission shapes: at the per-principal session cap with every
// session busy, admission answers the bounded-admission refusal (503 +
// Retry-After + JSON-RPC error, NO session minted) with ZERO extra backend
// dispatch; the per-server call cap admits exactly one backend dispatch at a
// time (a competing call queues, never double-dispatches), and a client
// disconnect cannot bypass that cap while a non-cooperative call still runs
// (ports of modern.test.ts "admission stays bounded" and "disconnect cannot
// bypass principal concurrency").
// ---------------------------------------------------------------------------

nativeTest(
  "MCP server: session admission at the per-principal cap is a 503 with Retry-After and zero extra dispatch",
  async () => {
    const { store, service } = await withService();
    const g = gate();
    let dispatches = 0;
    try {
      await service.registerTool(HOLD_READ, async () => {
        dispatches += 1;
        await g.promise;
        return { data: "held" };
      });
      const info = await service.startMcpServer({
        bearerTokens: [TOKEN],
        maxSessionsPerPrincipal: 1,
        idleTtlMs: 60_000,
      });
      const [client, transport] = newMcpClient(info.url, TOKEN);
      await client.connect(transport);

      const held = client.callTool({ name: "hold", arguments: {} });
      await waitFor(
        () => dispatches,
        (count) => count === 1,
      );

      // Same principal, every session busy: bounded-admission refusal —
      // never a dispatch, never a session.
      const refusal = await fetch(`${info.url}/`, {
        method: "POST",
        headers: rpcHeaders(TOKEN),
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 9,
          method: "initialize",
          params: {
            protocolVersion: "2025-06-18",
            capabilities: {},
            clientInfo: { name: "competitor", version: "1" },
          },
        }),
      });
      expect(refusal.status).toBe(503);
      expect(refusal.headers.get("retry-after")).toBe("5");
      expect(refusal.headers.get("mcp-session-id")).toBeNull();
      const refusalBody = (await refusal.json()) as {
        jsonrpc: string;
        error: { code: number };
      };
      expect(refusalBody.jsonrpc).toBe("2.0");
      expect(refusalBody.error.code).toBe(-32000);
      expect(dispatches).toBe(1);

      // Capacity returns once the busy session settles: the next initialize
      // succeeds by evicting the now-idle session.
      g.release();
      await held;
      const admitted = await fetch(`${info.url}/`, {
        method: "POST",
        headers: rpcHeaders(TOKEN),
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: 10,
          method: "initialize",
          params: {
            protocolVersion: "2025-06-18",
            capabilities: {},
            clientInfo: { name: "competitor", version: "1" },
          },
        }),
      });
      expect(admitted.status).toBe(200);
      expect(admitted.headers.get("mcp-session-id")).toBeString();

      await client.close().catch(() => undefined);
      await service.stopMcpServer();
    } finally {
      g.release();
      await service.close();
      await store.close();
    }
  },
  30_000,
);

nativeTest(
  "MCP server: the call cap dispatches one at a time — a competing call queues, never double-dispatches",
  async () => {
    const { store, service } = await withService();
    const g = gate();
    let dispatches = 0;
    try {
      await service.registerTool(HOLD_READ, async () => {
        dispatches += 1;
        await g.promise;
        return { data: "held" };
      });
      const info = await service.startMcpServer({
        bearerTokens: [TOKEN],
        maxConcurrentCalls: 1,
        idleTtlMs: 60_000,
      });
      const [client, transport] = newMcpClient(info.url, TOKEN);
      await client.connect(transport);

      const first = client.callTool({ name: "hold", arguments: {} });
      await waitFor(
        () => dispatches,
        (count) => count === 1,
      );
      const second = client.callTool({ name: "hold", arguments: {} });
      let secondSettled = false;
      void second.then(
        () => (secondSettled = true),
        () => (secondSettled = true),
      );
      // Real-clock window, unavoidable: the assertion is the ABSENCE of an
      // event (no settle, no backend entry) while the first call holds the
      // single permit — there is no signal to await for something that must
      // not happen. Same bounded race window the TS original used.
      await Bun.sleep(100);
      expect(secondSettled).toBe(false);
      expect(dispatches).toBe(1);

      g.release();
      const [firstResult, secondResult] = await Promise.all([first, second]);
      expect(dispatches).toBe(2);
      expect(JSON.stringify(firstResult.content)).toContain("held");
      expect(JSON.stringify(secondResult.content)).toContain("held");

      await client.close().catch(() => undefined);
      await service.stopMcpServer();
    } finally {
      g.release();
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// Group 12 (cont.) — the batch cap on the HTTP wire: a JSON-RPC batch over
// `maxBatchSize` is refused as ONE protocol error (400, code −32600) on the
// established session and dispatches NONE of its items (port of
// handler.test.ts "batch > maxBatchSize 400/−32600 zero-dispatch"; the
// crate-level pin in `tests/limits.rs` stays complementary — this asserts
// the shape the binding's own HTTP endpoint answers with).
// ---------------------------------------------------------------------------

nativeTest(
  "MCP server: a batch over maxBatchSize is refused as one −32600 with zero dispatch",
  async () => {
    const { store, service } = await withService();
    let dispatches = 0;
    try {
      await service.registerTool(ECHO_READ, async () => {
        dispatches += 1;
        return { data: "x" };
      });
      const info = await service.startMcpServer({
        bearerTokens: [TOKEN],
        maxBatchSize: 2,
      });
      try {
        const headers = rpcHeaders(TOKEN);
        const initialized = await fetch(`${info.url}/`, {
          method: "POST",
          headers,
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "initialize",
            params: {
              protocolVersion: "2025-06-18",
              capabilities: {},
              clientInfo: { name: "batch", version: "1" },
            },
          }),
        });
        expect(initialized.status).toBe(200);
        const sessionId = initialized.headers.get("mcp-session-id");
        expect(typeof sessionId).toBe("string");

        const batch = [1, 2, 3].map((id) => ({
          jsonrpc: "2.0",
          id,
          method: "tools/call",
          params: { name: "echo", arguments: { value: "over-cap" } },
        }));
        const refused = await fetch(`${info.url}/`, {
          method: "POST",
          headers: {
            ...headers,
            "mcp-session-id": sessionId as string,
            "mcp-protocol-version": "2025-06-18",
          },
          body: JSON.stringify(batch),
        });
        expect(refused.status).toBe(400);
        const body = (await refused.json()) as {
          jsonrpc: string;
          error?: { code: number };
        };
        // ONE protocol error for the whole batch — never per-item answers.
        expect(body.jsonrpc).toBe("2.0");
        expect(body.error?.code).toBe(-32600);
        expect(dispatches).toBe(0);
      } finally {
        await service.stopMcpServer().catch(() => undefined);
      }
    } finally {
      await service.close();
      await store.close();
    }
  },
  15_000,
);

/**
 * TS original ("disconnect cannot bypass principal concurrency while
 * noncooperative tool still runs") asserted the permit stays pinned while
 * the abandoned non-cooperative callback runs. The native design INVERTS
 * that leg deliberately: an aborted HTTP request cancels the backend call
 * (the groups-6 contract), so the abandoned callback is revoked — its
 * abort signal fires and the single call slot frees for the next dispatch.
 * The guarantee ported here is the native equivalent: the disconnect never
 * strands the session or leaves the abandoned call unrevoked, and no two
 * calls are EVER concurrently live at the cap (the live-overlap half is
 * pinned by the queueing test above).
 */
nativeTest(
  "MCP server: a client disconnect revokes the abandoned call and the next call proceeds",
  async () => {
    const { store, service } = await withService();
    const g = gate();
    let dispatches = 0;
    const abandoned: { signal: AbortSignal | undefined }[] = [];
    try {
      await service.registerTool(HOLD_READ, async (_input, request) => {
        dispatches += 1;
        if (dispatches === 1) abandoned.push({ signal: request.signal });
        // Non-cooperative: ignores its abort signal, like the TS original.
        await g.promise;
        return { data: "held" };
      });
      const info = await service.startMcpServer({
        bearerTokens: [TOKEN],
        maxConcurrentCalls: 1,
        idleTtlMs: 60_000,
      });
      const [client, transport] = newMcpClient(info.url, TOKEN);
      await client.connect(transport);

      const controller = new AbortController();
      const first = client.callTool(
        { name: "hold", arguments: {} },
        { signal: controller.signal },
      );
      await waitFor(
        () => dispatches,
        (count) => count === 1,
      );
      controller.abort();
      await first.catch(() => undefined);

      // The abandoned call is actively revoked, not silently dangling: the
      // router raises the callback's abort signal.
      await waitFor(
        () => abandoned[0]?.signal?.aborted === true,
        (aborted) => aborted,
      );

      // With the abandoned call revoked, the competing call proceeds —
      // exactly one backend call live at a time, the cap never overlapping
      // (queueing test above pins the not-yet-revoked case).
      const second = client.callTool({ name: "hold", arguments: {} });
      await waitFor(
        () => dispatches,
        (count) => count === 2,
      );
      g.release();
      const secondResult = await second;
      expect(JSON.stringify(secondResult.content)).toContain("held");

      await client.close().catch(() => undefined);
      await service.stopMcpServer();
    } finally {
      g.release();
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// Group 13 — the unauthenticated refusal is bodyless AND carries the
// `WWW-Authenticate` challenge, verbatim from the TS handler
// (`server.ts:750`): `Bearer realm="agentkit-mcp"`.
// ---------------------------------------------------------------------------

nativeTest(
  "MCP server: the bodyless 401 carries the Bearer WWW-Authenticate challenge",
  async () => {
    const { store, service } = await withService();
    try {
      await service.registerTool(ECHO_READ, async () => ({ data: "x" }));
      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      try {
        const anonymous = await fetch(`${info.url}/`, {
          method: "POST",
          body: "{}",
        });
        expect(anonymous.status).toBe(401);
        expect(await anonymous.text()).toBe("");
        expect(anonymous.headers.get("www-authenticate")).toBe(
          'Bearer realm="agentkit-mcp"',
        );
      } finally {
        await service.stopMcpServer().catch(() => undefined);
      }
    } finally {
      await service.close();
      await store.close();
    }
  },
  15_000,
);
