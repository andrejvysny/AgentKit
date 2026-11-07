import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeStore } from "../src/index.js";
import type { NativeMcpClientConfig } from "../src/mcp.js";
import { testServiceOptions } from "./helpers/test-profiles.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;
const token = "12345678901234567890123456789012";
const secret = "synthetic-mcp-credential";
const auth = {
  authorization: `Bearer ${token}`,
  "content-type": "application/json",
};

function resource() {
  let effects = 0;
  const credentials: string[] = [];
  const server = Bun.serve({
    port: 0,
    async fetch(request) {
      if (request.method !== "POST") return new Response(null, { status: 405 });
      credentials.push(request.headers.get("authorization") ?? "");
      const rpc = (await request.json()) as { id?: unknown; method?: string };
      let result: unknown;
      if (rpc.method === "initialize")
        result = {
          protocolVersion: "2025-11-25",
          capabilities: { tools: {} },
          serverInfo: { name: "resource", version: "1" },
        };
      else if (rpc.method === "tools/list")
        result = {
          tools: [{ name: "write", inputSchema: { type: "object" } }],
        };
      else if (rpc.method === "tools/call") {
        effects += 1;
        result = { content: [{ type: "text", text: "done" }] };
      } else return new Response(null, { status: 202 });
      return Response.json({ jsonrpc: "2.0", id: rpc.id, result });
    },
  });
  return {
    server,
    url: `http://127.0.0.1:${server.port}/mcp`,
    effects: () => effects,
    credentials,
  };
}

nativeTest(
  "durable HTTP MCP config uses one native manager and stores only vault references",
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "mcp-config-runtime-"));
    const path = join(dir, "store.sqlite");
    const remote = resource();
    const store = await NativeStore.open(path);
    const vault = new Map<string, string>();
    await store.mcpConfigureSecrets({
      get: (key) => vault.get(key) ?? null,
      set: (key, value) => {
        vault.set(key, value);
      },
      delete: (key) => {
        vault.delete(key);
      },
    });
    const manual = await store.mcpConnect({
      alias: "manual",
      transport: { kind: "http", url: remote.url },
    });
    const service = await store.createService(testServiceOptions());
    const db = new Database(path, { readonly: true });
    try {
      const { baseUrl } = await service.startHttpServer({
        bearerToken: token,
        actorId: "mcp-runtime",
      });
      expect(
        (await store.mcpListServers()).some(
          (item) => item.alias === "manual" && item.connected,
        ),
      ).toBe(true);
      const created = await fetch(`${baseUrl}/v1/mcp/servers`, {
        method: "POST",
        headers: auth,
        body: JSON.stringify({
          alias: "managed",
          transport: {
            kind: "http",
            url: remote.url,
            headers: { Authorization: `Bearer ${secret}` },
          },
        }),
      });
      expect(created.status).toBe(201);
      const publicRecord = (await created.json()) as {
        id: string;
        transport: unknown;
      };
      expect(JSON.stringify(publicRecord)).not.toContain(secret);
      const raw = db
        .query(
          "SELECT value FROM settings WHERE namespace='mcp-config' AND key='mcp-runtime'",
        )
        .get() as { value: string };
      expect(raw.value).not.toContain(secret);
      const declarations = JSON.parse(raw.value) as Array<
        NativeMcpClientConfig & { id: string }
      >;
      expect(
        Object.values(declarations[0]!.secretRefs ?? {}).every((ref) =>
          ref.startsWith("mcp/"),
        ),
      ).toBe(true);
      expect([...vault.values()]).toContain(`Bearer ${secret}`);
      const connection = await store.mcpConnect(declarations[0]!);
      expect(remote.credentials).toContain(`Bearer ${secret}`);
      const listed = await fetch(`${baseUrl}/v1/mcp/servers`, {
        headers: auth,
      });
      const rows = (await listed.json()) as Array<{
        alias: string;
        connection?: { status: string; toolCount: number };
      }>;
      expect(
        rows.find((item) => item.alias === "managed")?.connection,
      ).toMatchObject({ status: "connected", toolCount: 1 });
      const disabled = await fetch(
        `${baseUrl}/v1/mcp/servers/${publicRecord.id}`,
        {
          method: "PATCH",
          headers: auth,
          body: JSON.stringify({ enabled: false }),
        },
      );
      expect(disabled.status).toBe(200);
      await expect(
        store.mcpCallTool("managed", "write", {}, connection.tools[0]!.handle),
      ).rejects.toBeInstanceOf(Error);
      expect(remote.effects()).toBe(0);
      expect(
        (
          await store.mcpCallTool(
            "manual",
            "write",
            {},
            manual.tools[0]!.handle,
          )
        ).ok,
      ).toBe(true);
      const removed = await fetch(
        `${baseUrl}/v1/mcp/servers/${publicRecord.id}`,
        { method: "DELETE", headers: auth },
      );
      expect(removed.status).toBe(204);
      expect((await store.mcpListServers()).map((item) => item.alias)).toEqual([
        "manual",
      ]);
    } finally {
      db.close();
      await service.close();
      await store.close();
      remote.server.stop(true);
      rmSync(dir, { recursive: true, force: true });
    }
  },
);

nativeTest(
  "durable alias conflict preserves manual session and stored declarations",
  async () => {
    const remote = resource();
    const other = resource();
    const store = await NativeStore.inMemory();
    const manual = await store.mcpConnect({
      alias: "notes",
      transport: { kind: "http", url: remote.url },
    });
    const service = await store.createService(testServiceOptions());
    try {
      const { baseUrl } = await service.startHttpServer({
        bearerToken: token,
        actorId: "mcp-conflict",
      });
      const result = await fetch(`${baseUrl}/v1/mcp/servers`, {
        method: "POST",
        headers: auth,
        body: JSON.stringify({
          alias: "notes",
          transport: { kind: "http", url: other.url },
        }),
      });
      expect(result.status).toBe(409);
      const records = await fetch(`${baseUrl}/v1/mcp/servers`, {
        headers: auth,
      });
      expect(await records.json()).toEqual([]);
      expect(
        (await store.mcpCallTool("notes", "write", {}, manual.tools[0]!.handle))
          .ok,
      ).toBe(true);
      expect(remote.effects()).toBe(1);
      expect(other.effects()).toBe(0);
    } finally {
      await service.close();
      await store.close();
      remote.server.stop(true);
      other.server.stop(true);
    }
  },
);

nativeTest(
  "HTTP MCP literals fail closed when the store has no configured vault",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService(testServiceOptions());
    try {
      const { baseUrl } = await service.startHttpServer({
        bearerToken: token,
        actorId: "mcp-no-vault",
      });
      const result = await fetch(`${baseUrl}/v1/mcp/servers`, {
        method: "POST",
        headers: auth,
        body: JSON.stringify({
          alias: "refused",
          transport: {
            kind: "http",
            url: "https://resource.test/mcp",
            headers: { Authorization: secret },
          },
        }),
      });
      expect(result.status).toBe(501);
      expect(await result.text()).not.toContain(secret);
      const records = await fetch(`${baseUrl}/v1/mcp/servers`, {
        headers: auth,
      });
      expect(await records.json()).toEqual([]);
    } finally {
      await service.close();
      await store.close();
    }
  },
);
