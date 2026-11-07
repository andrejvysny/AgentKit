import { testServiceOptions } from "./helpers/test-profiles.js";
/**
 * Binding hardening: credential isolation across stores, mid-run credential
 * rotation, the MCP write gate's stricter-of classification, loopback-only
 * listeners, close() behaviour, and lossy number conversions.
 */
import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Client as LegacyClient } from "mcp-sdk-v1/client/index.js";
import { StreamableHTTPClientTransport as LegacyTransport } from "mcp-sdk-v1/client/streamableHttp.js";
import { NativeSessionService, NativeStore } from "../src/index.js";
import type { ToolDefinitionJson } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

const TOKEN = "12345678901234567890123456789012";

async function codeOf(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
  } catch (error) {
    return (error as { code?: string }).code ?? String(error);
  }
  return "no-error";
}

async function openStore(): Promise<NativeStore> {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-hardening-"));
  return NativeStore.open(join(dir, "store.sqlite"));
}

// ---------------------------------------------------------------------------
// N1: durable store and routing scopes isolate identical logical keys.
// ---------------------------------------------------------------------------

nativeTest(
  "keyed provider connections isolate stores and mint fresh endpoint identities",
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "agentkit-hardening-keys-"));
    const pathA = join(dir, "a.sqlite");
    const pathB = join(dir, "b.sqlite");
    const storeA = await NativeStore.open(pathA);
    const storeB = await NativeStore.open(pathB);
    const ids = new Set<string>();
    const register = async (
      storePath: string,
      endpoint: string,
      secret: string,
    ) => {
      const id = await NativeSessionService.registerProviderConnection(
        endpoint,
        secret,
        {
          connectionKey: "hardening-shared-key",
          storePath,
          accountScope: "trusted-account",
        },
      );
      ids.add(id);
      return id;
    };
    try {
      const first = await register(pathA, "https://one.test/v1", "sk-a");
      const independent = await register(
        pathB,
        "https://one.test/v1",
        "sk-other",
      );
      expect(independent).not.toBe(first);
      expect(await register(pathA, "https://one.test/v1", "sk-rotated")).toBe(
        first,
      );
      const changed = await register(
        pathA,
        "https://two.test/v1",
        "sk-rotated",
      );
      expect(changed).not.toBe(first);
      expect(await register(pathB, "https://one.test/v1", "sk-other")).toBe(
        independent,
      );
      expect(await NativeSessionService.revokeProviderConnection(changed)).toBe(
        true,
      );
      expect(await register(pathA, "https://two.test/v1", "sk-restored")).toBe(
        changed,
      );
      expect(await register(pathB, "https://one.test/v1", "sk-other")).toBe(
        independent,
      );
    } finally {
      for (const id of ids)
        await NativeSessionService.revokeProviderConnection(id);
      await storeA.close();
      await storeB.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// N12: listeners refuse non-loopback hosts unless explicitly opted in.
// ---------------------------------------------------------------------------

nativeTest(
  "HTTP and MCP servers refuse non-loopback binds unless allowNonLoopback",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService(testServiceOptions());
    try {
      for (const host of ["0.0.0.0", "::", "example.invalid"]) {
        expect(
          await codeOf(() =>
            service.startHttpServer({ host, bearerToken: TOKEN }),
          ),
        ).toBe("non_loopback_bind_refused");
        expect(
          await codeOf(() =>
            service.startMcpServer({ host, bearerTokens: [TOKEN] }),
          ),
        ).toBe("non_loopback_bind_refused");
      }
      const loopback = await service.startHttpServer({
        host: "localhost",
        bearerToken: TOKEN,
      });
      expect(loopback.port).toBeGreaterThan(0);
      await service.stopHttpServer();
      const opened = await service.startHttpServer({
        host: "0.0.0.0",
        allowNonLoopback: true,
        bearerToken: TOKEN,
      });
      expect(opened.port).toBeGreaterThan(0);
      await service.stopHttpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// N4: the MCP write gate classifies by the stricter of capability and
// effect, exactly like the run-scoped JS guard.
// ---------------------------------------------------------------------------

const schema = { type: "object" } as const;
const SAFE_READ: ToolDefinitionJson = {
  name: "safe_read",
  effect: "read",
  capability: "fs.read",
  description: "read",
  inputSchema: schema,
};
const READ_EXEC: ToolDefinitionJson = {
  name: "read_exec",
  effect: "read",
  capability: "process.exec",
  description: "declares read, can exec",
  inputSchema: schema,
};
const WRITE_FS_READ: ToolDefinitionJson = {
  name: "write_fsread",
  effect: "write",
  capability: "fs.read",
  description: "declares write, claims a read capability",
  inputSchema: schema,
};

nativeTest(
  "MCP server hides and refuses exec/write tools whatever mix of effect and capability they declare",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService(testServiceOptions());
    const calls: string[] = [];
    try {
      for (const definition of [SAFE_READ, READ_EXEC, WRITE_FS_READ])
        await service.registerTool(definition, async () => {
          calls.push(definition.name);
          return { data: "ran" };
        });
      const info = await service.startMcpServer({ bearerTokens: [TOKEN] });
      const client = new LegacyClient({ name: "hardening", version: "1" });
      const transport = new LegacyTransport(new URL(info.url), {
        requestInit: { headers: { authorization: `Bearer ${TOKEN}` } },
      });
      try {
        await client.connect(transport);
        const listed = await client.listTools();
        expect(listed.tools.map((tool) => tool.name)).toEqual(["safe_read"]);
        for (const name of ["read_exec", "write_fsread"])
          await expect(
            client.callTool({ name, arguments: {} }),
          ).rejects.toThrow();
        expect(calls).toEqual([]);
      } finally {
        await client.close().catch(() => undefined);
      }
      await service.stopMcpServer();

      const open = await service.startMcpServer({
        bearerTokens: [TOKEN],
        writesEnabled: true,
      });
      const writer = new LegacyClient({ name: "hardening-w", version: "1" });
      const writerTransport = new LegacyTransport(new URL(open.url), {
        requestInit: { headers: { authorization: `Bearer ${TOKEN}` } },
      });
      try {
        await writer.connect(writerTransport);
        const listed = await writer.listTools();
        expect(listed.tools.map((tool) => tool.name).sort()).toEqual([
          "read_exec",
          "safe_read",
          "write_fsread",
        ]);
      } finally {
        await writer.close().catch(() => undefined);
      }
    } finally {
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// N14: lossy number conversions fail closed.
// ---------------------------------------------------------------------------

nativeTest(
  "integer arguments reject non-finite, fractional, and out-of-range numbers",
  async () => {
    const store = await openStore();
    const service = await store.createService(testServiceOptions());
    try {
      for (const bad of [Number.NaN, 1.5, -1, 0, 2 ** 40, Infinity])
        expect(
          await codeOf(() => service.advisorFindings("any-session", bad)),
        ).toBe("invalid_input");
      service.setProvider(() => ({ content: "ok", finishReason: "stop" }));
      await service.createSession({
        sessionId: "s1",
        profileId: "p1",
        model: "m",
      });
      const sent = await service.send("s1", { content: "hi" });
      if (sent.kind !== "turn") throw new Error("expected a turn");
      const subscription = await service.subscribeTurn(sent.turnId, 0);
      for (const bad of [Number.NaN, 0, 65, 2.5])
        expect(await codeOf(() => subscription.poll(bad, 1))).toBe(
          "invalid_input",
        );
      subscription.cancel();
    } finally {
      await service.close();
      await store.close();
    }
  },
  30_000,
);

// ---------------------------------------------------------------------------
// N5/N9: close() cancels parked subscriptions and stays idempotent.
// ---------------------------------------------------------------------------

nativeTest(
  "cancel() wakes a parked poll at once and close() leaves no live subscription",
  async () => {
    const store = await NativeStore.inMemory();
    const service = await store.createService(testServiceOptions());
    service.setProvider(() => new Promise(() => undefined));
    try {
      await service.createSession({
        sessionId: "s1",
        profileId: "p1",
        model: "m",
      });
      const sent = await service.send("s1", { content: "hang" });
      if (sent.kind !== "turn") throw new Error("expected a turn");
      // A cursor far beyond the run's events: the poll can only park.
      const parked = await service.subscribeTurn(sent.turnId, 1_000_000);
      const pending = parked.poll(1, 20_000);
      await Bun.sleep(100);
      const started = Date.now();
      parked.cancel();
      const result = await pending;
      expect(result.done).toBe(true);
      expect(Date.now() - started).toBeLessThan(2_000);
      expect(parked.done).toBe(true);
      // A second subscription is cancelled by close() itself.
      const other = await service.subscribeTurn(sent.turnId, 1_000_000);
      const otherPending = other.poll(1, 20_000);
      await Bun.sleep(50);
      await service.close();
      expect((await otherPending).done).toBe(true);
      await service.close();
    } finally {
      await store.close();
    }
  },
  30_000,
);

nativeTest(
  "a dropped service handle with a running HTTP server keeps serving until close()",
  async () => {
    const store = await NativeStore.inMemory();
    const started = await (async () => {
      const service = await store.createService(testServiceOptions());
      const info = await service.startHttpServer({ bearerToken: TOKEN });
      // The only reference to `service` ends with this scope.
      return info;
    })();
    for (let round = 0; round < 3; round += 1) {
      Bun.gc(true);
      await Bun.sleep(50);
    }
    const response = await fetch(`${started.baseUrl}/v1/ping`, {
      headers: { authorization: `Bearer ${TOKEN}` },
    });
    // Any HTTP answer (even a 404 problem) proves the listener is alive.
    expect(response.status).toBeGreaterThan(0);
    await response.text();
    await store.close();
  },
  30_000,
);

nativeTest(
  "concurrent keyed registrations and rotations of one id end consistent",
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "agentkit-hardening-rot-"));
    const path = join(dir, "store.sqlite");
    const store = await NativeStore.open(path);
    try {
      const options = { connectionKey: "rotating-key", storePath: path };
      const ids = await Promise.all(
        Array.from({ length: 8 }, (_, index) =>
          NativeSessionService.registerProviderConnection(
            "https://rot.test/v1",
            `sk-${index}`,
            options,
          ),
        ),
      );
      expect(new Set(ids).size).toBe(1);
      // Still registrable (same base URL) and revocable exactly once.
      expect(
        await NativeSessionService.registerProviderConnection(
          "https://rot.test/v1",
          "sk-final",
          options,
        ),
      ).toBe(ids[0] as string);
      expect(await NativeSessionService.revokeProviderConnection(ids[0]!)).toBe(
        true,
      );
      expect(await NativeSessionService.revokeProviderConnection(ids[0]!)).toBe(
        false,
      );
    } finally {
      await store.close();
    }
  },
  30_000,
);
