import { expect, test } from "bun:test";
import { Database } from "bun:sqlite";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NativeStore } from "../src/index.js";
import type {
  McpOAuthCallbacks,
  McpOAuthHttpResponse,
} from "../src/callback-types.js";
import type { NativeManagedOAuthOptions } from "../src/mcp.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;
const fixture = join(import.meta.dir, "fixtures", "mcp-hardening-fixture.ts");
const config = (alias: string, env: Record<string, string> = {}) => ({
  alias,
  transport: {
    kind: "stdio" as const,
    command: process.execPath,
    args: [fixture],
    env,
  },
});
async function waitFor(predicate: () => boolean): Promise<void> {
  for (let count = 0; count < 400; count += 1) {
    if (predicate()) return;
    await Bun.sleep(5);
  }
  throw new Error("MCP fixture did not reach expected state");
}

nativeTest(
  "MCP pending initial connection cannot publish after disconnect",
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "mcp-disconnect-"));
    const ready = join(dir, "ready");
    const store = await NativeStore.inMemory();
    try {
      const pending = store.mcpConnect(
        config("pending", { READY_FILE: ready, INIT_DELAY_MS: "1000" }),
      );
      const outcome = pending.then(
        () => "published",
        () => "cancelled",
      );
      await waitFor(() => existsSync(ready));
      await store.mcpDisconnect("pending");
      expect(await outcome).toBe("cancelled");
      expect(await store.mcpListServers()).toEqual([]);
    } finally {
      await store.close();
      rmSync(dir, { recursive: true, force: true });
    }
  },
);

nativeTest(
  "MCP close during durable fence allocation never recreates registry",
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "mcp-close-"));
    const path = join(dir, "store.sqlite");
    const ready = join(dir, "ready");
    const store = await NativeStore.open(path);
    const blocker = new Database(path);
    try {
      blocker.exec("BEGIN IMMEDIATE");
      const pending = store.mcpConnect(
        config("pending", { READY_FILE: ready }),
      );
      const outcome = pending.then(
        () => "published",
        () => "cancelled",
      );
      await Bun.sleep(25);
      const closing = store.close();
      await Bun.sleep(25);
      blocker.exec("ROLLBACK");
      await closing;
      expect(await outcome).toBe("cancelled");
      expect(existsSync(ready)).toBe(false);
    } finally {
      blocker.close();
      await store.close();
      rmSync(dir, { recursive: true, force: true });
    }
  },
);

nativeTest(
  "MCP callback helper cannot disconnect replacement registration",
  async () => {
    const store = await NativeStore.inMemory();
    try {
      const first = await store.mcpConnectWithCallbacks(
        config("bound"),
        () => ({ action: "decline" }),
      );
      await store.mcpDisconnect("bound");
      const second = await store.mcpConnect(config("bound"));
      await expect(first.disconnect()).rejects.toMatchObject({
        code: "mcp_authority_revoked",
      });
      const result = await store.mcpCallTool(
        "bound",
        "echo",
        {},
        second.tools[0]!.handle,
      );
      expect(result.ok).toBe(true);
    } finally {
      await store.close();
    }
  },
);

nativeTest(
  "native elicitation reaches explicit host callback with a cancellable scope",
  async () => {
    const store = await NativeStore.inMemory();
    let aborted = false;
    let entered = false;
    try {
      const connection = await store.mcpConnectWithCallbacks(
        config("interactive"),
        async (request) => {
          expect(request.alias).toBe("interactive");
          entered = true;
          await new Promise<void>((resolve) =>
            request.signal.addEventListener(
              "abort",
              () => {
                aborted = true;
                resolve();
              },
              { once: true },
            ),
          );
          return { action: "cancel" };
        },
      );
      const controller = new AbortController();
      const pending = store.mcpCallTool(
        "interactive",
        "ask",
        {},
        connection.tools[0]!.handle,
        controller.signal,
      );
      const outcome = pending.then(
        () => "settled",
        () => "cancelled",
      );
      await waitFor(() => entered);
      controller.abort();
      expect(await outcome).toBe("cancelled");
      await waitFor(() => aborted);
      expect(
        (
          await store.mcpCallTool(
            "interactive",
            "echo",
            {},
            connection.tools[0]!.handle,
          )
        ).ok,
      ).toBe(true);
      await connection.disconnect();
    } finally {
      await store.close();
    }
  },
);

function oauthFixture(): {
  options: NativeManagedOAuthOptions;
  callbacks: McpOAuthCallbacks;
  vault: Map<string, string>;
  redirects: string[];
  effects: () => number;
} {
  const vault = new Map<string, string>();
  const redirects: string[] = [];
  let effects = 0;
  const options: NativeManagedOAuthOptions = {
    appNamespace: crypto.randomUUID(),
    generation: 1,
    connectionId: "connection",
    accountId: "trusted-account",
    resourceUrl: "https://resource.test/mcp",
    redirectUrl: "onemind://oauth/callback",
    allowedOrigins: ["https://resource.test", "https://issuer.test"],
    clientName: "Native test",
    scopes: ["read"],
    httpCallback: true,
  };
  const callbacks: McpOAuthCallbacks = {
    secrets: {
      get: (key) => vault.get(key) ?? null,
      set: (key, value) => {
        vault.set(key, value);
      },
      delete: (key) => {
        vault.delete(key);
      },
    },
    redirect: ({ url }) => {
      redirects.push(url);
    },
    http: (request): McpOAuthHttpResponse => {
      const url = new URL(request.url);
      let status = 200;
      let body: unknown;
      if (
        url.hostname === "resource.test" &&
        url.pathname.includes(".well-known")
      ) {
        body = {
          resource: options.resourceUrl,
          authorization_servers: ["https://issuer.test"],
        };
      } else if (
        url.hostname === "issuer.test" &&
        url.pathname.includes(".well-known")
      ) {
        body = {
          issuer: "https://issuer.test",
          authorization_endpoint: "https://issuer.test/authorize",
          token_endpoint: "https://issuer.test/token",
          registration_endpoint: "https://issuer.test/register",
          response_types_supported: ["code"],
          code_challenge_methods_supported: ["S256"],
          authorization_response_iss_parameter_supported: true,
        };
      } else if (url.pathname === "/register") {
        status = 201;
        body = {
          client_id: "native-client",
          client_secret: "synthetic-client-secret",
          redirect_uris: [options.redirectUrl],
        };
      } else if (url.pathname === "/token") {
        body = {
          access_token: "synthetic-access-token",
          refresh_token: "synthetic-refresh-token",
          token_type: "Bearer",
          expires_in: 3600,
        };
      } else if (request.method === "GET") {
        status = 401;
        return {
          status,
          headers: {
            "content-type": "application/json",
            "www-authenticate":
              'Bearer resource_metadata="https://resource.test/.well-known/oauth-protected-resource/mcp"',
          },
          body: "{}",
        };
      } else {
        const rpc = JSON.parse(request.body) as {
          id?: unknown;
          method?: string;
        };
        if (rpc.method === "initialize")
          body = {
            jsonrpc: "2.0",
            id: rpc.id,
            result: {
              protocolVersion: "2025-11-25",
              capabilities: { tools: {} },
              serverInfo: { name: "native-oauth", version: "1" },
            },
          };
        else if (rpc.method === "tools/list")
          body = {
            jsonrpc: "2.0",
            id: rpc.id,
            result: {
              tools: [{ name: "write", inputSchema: { type: "object" } }],
            },
          };
        else if (rpc.method === "tools/call") {
          effects += 1;
          status = 401;
          body = {
            jsonrpc: "2.0",
            id: rpc.id,
            error: { code: -32001, message: "authorization required" },
          };
        } else {
          status = 202;
          body = null;
        }
      }
      return {
        status,
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      };
    },
  };
  return { options, callbacks, vault, redirects, effects: () => effects };
}

nativeTest(
  "native managed OAuth composes vault/browser ports and never replays401 mutation",
  async () => {
    const store = await NativeStore.inMemory();
    const fixture = oauthFixture();
    const config = {
      alias: "oauth",
      transport: { kind: "http" as const, url: fixture.options.resourceUrl },
    };
    try {
      await expect(
        store.mcpConnectWithOAuth(config, fixture.options, fixture.callbacks),
      ).rejects.toMatchObject({ code: "mcp_auth_required" });
      expect(fixture.redirects).toHaveLength(1);
      const authorize = new URL(fixture.redirects[0]!);
      expect(authorize.searchParams.get("code_challenge_method")).toBe("S256");
      const callback = new URL(fixture.options.redirectUrl);
      callback.searchParams.set("code", "synthetic-code");
      callback.searchParams.set("state", authorize.searchParams.get("state")!);
      callback.searchParams.set("iss", "https://issuer.test");
      await store.mcpCompleteOAuth(
        fixture.options,
        callback.href,
        fixture.callbacks,
      );
      const connection = await store.mcpConnectWithOAuth(
        config,
        fixture.options,
        fixture.callbacks,
      );
      await expect(
        store.mcpCallTool("oauth", "write", {}, connection.tools[0]!.handle),
      ).rejects.toBeInstanceOf(Error);
      expect(fixture.effects()).toBe(1);
      expect(
        [...fixture.vault.values()].some((value) =>
          value.includes("synthetic-client-secret"),
        ),
      ).toBe(true);
      await store.mcpRevokeOAuth(fixture.options, fixture.callbacks);
      expect(
        [...fixture.vault.keys()].some((key) => key.endsWith("revoked")),
      ).toBe(true);
    } finally {
      await store.close();
    }
  },
);
