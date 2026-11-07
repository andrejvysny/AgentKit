/**
 * Typed surface for the native MCP binding (AGENTKIT-56): the Rust-hosted
 * MCP SERVER on `NativeSessionService`, and the Rust-managed MCP CLIENT
 * connections on `NativeStore`. Both directions are hosted by the Rust
 * `agentkit-mcp` crate — this file only types the JSON contract and wires
 * the convenience registration lane; it holds no session, transport, or
 * policy logic.
 *
 * Effect authority is HOST-side in both directions:
 * - CLIENT: a descriptor's `effect` comes SOLELY from the host config's
 *   `readOnly` allowance (remote `readOnlyHint` annotations are
 *   descriptive only). Register the returned descriptors through the
 *   existing `registerTool` path — `registerMcpTools` below does exactly
 *   that — so the Rust `BindingAuthority` + `JsToolGuard` chain keeps
 *   judging every remote-backed call; `mcpCallTool` from the tool lane is
 *   the executor, never a bypass around those checks.
 * - SERVER: a remote client can only reach this service's registered JS
 *   tools; write-effect tools stay invisible and uncallable until the
 *   host explicitly starts the server with `writesEnabled: true`.
 */

import type {
  McpElicitationHandler,
  McpOAuthCallbacks,
  ToolDefinitionJson,
  ToolResultJson,
  ToolRunFunction,
} from "./callback-types.js";
import { CallbackRouter } from "./callback-router.js";

export interface NativeMcpCallbackStore {
  mcpConnectWithCallbacks(
    configJson: string,
    deliver: (error: Error | null, payload: string) => void,
  ): Promise<string>;
  mcpDisconnect(alias: string): Promise<void>;
  mcpDisconnectBound(alias: string, expectedHandle: string): Promise<void>;
}

/** One callback router per connection; closing it aborts outstanding user prompts. */
export async function connectMcpWithCallbacks(
  store: NativeMcpCallbackStore,
  config: NativeMcpClientConfig,
  elicitation: McpElicitationHandler,
): Promise<NativeMcpConnection & { disconnect(): Promise<void> }> {
  const router = new CallbackRouter();
  router.setMcpElicitation(elicitation);
  try {
    const result = await store.mcpConnectWithCallbacks(
      JSON.stringify(config),
      (error, payload) => {
        if (error === null) void router.handle(payload);
      },
    );
    const connection = JSON.parse(result) as NativeMcpConnection;
    return {
      ...connection,
      async disconnect() {
        try {
          await store.mcpDisconnectBound(
            connection.alias,
            connectionHandle(connection),
          );
        } finally {
          router.close();
        }
      },
    };
  } catch (error) {
    router.close();
    throw error;
  }
}

function connectionHandle(connection: NativeMcpConnection): string {
  const handle = connection.handle ?? connection.tools[0]?.handle;
  if (!handle)
    throw new Error("MCP connection is missing its lifecycle handle");
  return handle;
}

export interface NativeManagedOAuthOptions {
  /** Stable, unique application/store namespace; never a global account selection. */
  appNamespace: string;
  /** Persisted connection generation. Advance on reauthorization or revocation. */
  generation: number;
  connectionId: string;
  accountId: string;
  resourceUrl: string;
  redirectUrl: string;
  allowedOrigins: string[];
  clientId?: string;
  /** Transient input; only the explicit vault port stores it. */
  clientSecret?: string;
  clientName: string;
  scopes: string[];
  /** Use the explicit trusted HTTP callback for OAuth and resource requests. */
  httpCallback?: boolean;
}

export interface NativeMcpOAuthStore {
  mcpConnectWithOAuth(
    configJson: string,
    oauthOptionsJson: string,
    deliver: (error: Error | null, payload: string) => void,
  ): Promise<string>;
  mcpCompleteOAuth(
    oauthOptionsJson: string,
    callbackUrl: string,
    deliver: (error: Error | null, payload: string) => void,
  ): Promise<void>;
  mcpRevokeOAuth(
    oauthOptionsJson: string,
    deliver: (error: Error | null, payload: string) => void,
  ): Promise<void>;
  mcpDisconnectBound(alias: string, expectedHandle: string): Promise<void>;
}

export async function connectMcpWithOAuth(
  store: NativeMcpOAuthStore,
  config: NativeMcpClientConfig,
  options: NativeManagedOAuthOptions,
  callbacks: McpOAuthCallbacks & { elicitation?: McpElicitationHandler },
): Promise<NativeMcpConnection & { disconnect(): Promise<void> }> {
  const router = new CallbackRouter();
  router.setMcpOAuth(callbacks);
  if (callbacks.elicitation) router.setMcpElicitation(callbacks.elicitation);
  try {
    const result = await store.mcpConnectWithOAuth(
      JSON.stringify({
        ...config,
        elicitation: callbacks.elicitation !== undefined,
      }),
      JSON.stringify(options),
      (error, payload) => {
        if (error === null) void router.handle(payload);
      },
    );
    const connection = JSON.parse(result) as NativeMcpConnection;
    return {
      ...connection,
      async disconnect() {
        try {
          await store.mcpDisconnectBound(
            connection.alias,
            connectionHandle(connection),
          );
        } finally {
          router.close();
        }
      },
    };
  } catch (error) {
    router.close();
    throw error;
  }
}

export async function completeMcpOAuth(
  store: NativeMcpOAuthStore,
  options: NativeManagedOAuthOptions,
  callbackUrl: string,
  callbacks: McpOAuthCallbacks,
): Promise<void> {
  const router = new CallbackRouter();
  router.setMcpOAuth(callbacks);
  try {
    await store.mcpCompleteOAuth(
      JSON.stringify(options),
      callbackUrl,
      (error, payload) => {
        if (error === null) void router.handle(payload);
      },
    );
  } finally {
    router.close();
  }
}

export async function revokeMcpOAuth(
  store: NativeMcpOAuthStore,
  options: NativeManagedOAuthOptions,
  callbacks: McpOAuthCallbacks,
): Promise<void> {
  const router = new CallbackRouter();
  router.setMcpOAuth(callbacks);
  try {
    await store.mcpRevokeOAuth(JSON.stringify(options), (error, payload) => {
      if (error === null) void router.handle(payload);
    });
  } finally {
    router.close();
  }
}

// ---------------------------------------------------------------------------
// Server (this service hosts an MCP server; remote clients connect to it)
// ---------------------------------------------------------------------------

/**
 * Options for `NativeSessionService.startMcpServer` (mirrors
 * `startMcpServer` in `rust/crates/napi/src/mcp_server.rs`). The server is
 * the Rust streamable-HTTP MCP engine (official `rmcp` SDK) mounted on a
 * listener bound the same way as the desktop REST server. Only
 * `bearerTokens` is required. Authentication is a constant-time digest
 * compare against the configured tokens; sessions are principal-bound per
 * token.
 */
export interface NativeMcpServerOptions {
  /** Interface to bind. Default `127.0.0.1`. Only loopback hosts
   * (`127.0.0.0/8`, `::1`, `localhost`) are accepted unless
   * `allowNonLoopback` is set. */
  host?: string;
  /** Opt in to binding a non-loopback host. Without it such a host fails
   * with `non_loopback_bind_refused`. Default false. */
  allowNonLoopback?: boolean;
  /** Port to bind; `0` (the default) asks the OS for an ephemeral port. */
  port?: number;
  /** REQUIRED, non-empty. Every token must be at least 32 bytes of
   * printable ASCII without whitespace; clients present one as
   * `Authorization: Bearer <token>`. */
  bearerTokens: string[];
  /** Host header allow list (Host-header rebinding defense). Default
   * `["localhost", "127.0.0.1", "[::1]"]`. */
  allowedHosts?: string[];
  /** Origin allow list for browser clients; when absent, no Origin is
   * specially permitted. */
  allowedOrigins?: string[];
  /** Open write-effect tools to remote clients (list AND call). Default
   * FALSE — remote clients can only see and invoke read-effect tools
   * unless this is explicitly enabled. */
  writesEnabled?: boolean;
  /** Concurrent tool executions across all sessions. Default 4. */
  maxConcurrentCalls?: number;
  /** Maximum JSON-RPC batch size. Default 8. */
  maxBatchSize?: number;
  /** Per-call deadline in milliseconds. Default 120000. */
  callDeadlineMs?: number;
  /** Idle session TTL in milliseconds. Default 1800000. */
  idleTtlMs?: number;
  /** Raw request body cap in bytes. Default 4194304. */
  maxBodyBytes?: number;
  /** Sessions per bearer principal. Default 64. */
  maxSessionsPerPrincipal?: number;
}

/** Where a started MCP server actually listens (`url` is mountable as-is
 * by any streamable-HTTP client, e.g. `new URL(info.url)`). */
export interface NativeMcpServerInfo {
  port: number;
  url: string;
}

// ---------------------------------------------------------------------------
// Client (this process connects to remote MCP servers)
// ---------------------------------------------------------------------------

/** Stdio transport: the crate spawns `command` with `args` and only the
 * explicitly supplied environment; inherited variables are not forwarded. */
export interface NativeMcpStdioTransport {
  kind: "stdio";
  command: string;
  args?: string[];
  env?: Record<string, string>;
}

/** Streamable-HTTP transport. */
export interface NativeMcpHttpTransport {
  kind: "http";
  url: string;
  headers?: Record<string, string>;
}

export type NativeMcpTransport =
  | NativeMcpStdioTransport
  | NativeMcpHttpTransport;

/**
 * One MCP client declaration (the crate's `ServerConfig` shape; see
 * `rust/crates/mcp/src/manager/config.rs`). Manual declarations are ephemeral;
 * HTTP management restores durable declarations through the same manager.
 * Resolved secret material is stored only through the configured host vault.
 */
export interface NativeMcpClientConfig {
  /** Host attestation of the selected model's image/audio support. */
  modelContentCapabilities?: { image?: boolean; audio?: boolean };
  /** Server identity: lowercase ASCII letters, digits and `-`. Used in
   * every tool's canonical capability `mcp.<alias>.<tool>`. */
  alias: string;
  transport: NativeMcpTransport;
  /** Placeholder to host-vault reference. Missing vault values fail the
   * connection with `mcp_secret_missing`. */
  secretRefs?: Record<string, string>;
  /** Default true. `mcpConnect` refuses a disabled server outright. */
  enabled?: boolean;
  /** Remote name → effective name rewrites applied to contributed tools. */
  toolAliases?: Record<string, string>;
  /** HOST-side read-only allowance. This trusted flag is the ONLY thing
   * that decides the contributed `effect` — remote annotations never do.
   * Default false → every descriptor is `effect:"write"` and the normal
   * write-tool authority (plan mode, approvals) applies. */
  readOnly?: boolean;
  /** Crate resilience knobs (request/connect timeouts, connect retries,
   * circuit and reconnect backoff). Optional; crate defaults apply. */
  resilience?: Record<string, unknown>;
}

/** One contributed remote tool: the portable tool descriptor contract
 * (directly registerable via `NativeSessionService.registerTool`) plus
 * `remoteName`, the unmodified server-side name for `mcpCallTool`, and the
 * registration `handle` that `mcpCallTool` requires back. */
export interface NativeMcpToolDescriptor extends ToolDefinitionJson {
  /** Unmodified name as the remote server declared it. */
  remoteName: string;
  /** The registration that minted this descriptor (`"<alias>#<n>"`). Pass it
   * as `mcpCallTool`'s fourth argument: a descriptor from a superseded
   * registration of the same alias is refused with `mcp_authority_revoked`
   * instead of executing the replacement server's callback. */
  handle: string;
}

/** What one `mcpConnect` established. */
export interface NativeMcpConnection {
  alias: string;
  handle?: string;
  tools: NativeMcpToolDescriptor[];
}

/** One row of `mcpListServers`. `connected` is the crate manager's live
 * transport view; a dropped connection reports false and reconnects on
 * the next host call. */
export interface NativeMcpServerStatus {
  alias: string;
  kind: "stdio" | "http";
  enabled: boolean;
  readOnly: boolean;
  connected: boolean;
  /** How many times this alias has been (re)registered on this store; it
   * never decreases, so `1` means the descriptors are still the original
   * registration's. */
  registration: number;
  /** The current registration's handle (`"<alias>#<n>"`), or null when the
   * alias is declared but has never connected. */
  handle: string | null;
  tools: number;
}

/** Structural view of the session-service register lane (satisfied by
 * `NativeSessionService`) so this module stays import-free of the class. */
export interface McpToolRegistrar {
  registerTool(
    definition: ToolDefinitionJson,
    run: ToolRunFunction,
  ): Promise<void>;
}

/** Structural view of the store's client lane (satisfied by
 * `NativeStore`). `handle` is the descriptor's registration handle; omit it
 * only for an alias that has never been re-registered, otherwise the call is
 * refused with `mcp_authority_revoked`. */
export interface McpToolCaller {
  mcpCallTool(
    alias: string,
    toolName: string,
    args: unknown,
    handle?: string,
    /** Aborts the remote call with the originating invocation (the tool ctx
     * `signal`): a queued call is never written, an in-flight call is torn
     * down. Omit only from host code that owns no invocation lifecycle. */
    signal?: AbortSignal,
  ): Promise<ToolResultJson>;
}

/**
 * Register every descriptor of a connected MCP server through the
 * EXISTING `registerTool` path — the authority lane. Each registered
 * tool's execution invokes `mcpCallTool` for the descriptor's
 * `remoteName` WITH THAT DESCRIPTOR'S OWN HANDLE and the invocation's
 * abort `signal` (the tool ctx `signal` threaded through), so the Rust
 * `BindingAuthority` + `JsToolGuard` chain (plan mode, approvals, effect
 * taxonomy) judges the call exactly as it judges any other JS tool, a
 * cancelled invocation tears the remote call down instead of dispatching
 * after the run settled, and a
 * callback from a superseded registration of the same alias can never be
 * reached by a descriptor the host authorized earlier. The effect on the
 * descriptor came from the host `readOnly` config alone. Returns the
 * descriptors it registered, unchanged.
 */
export async function registerMcpTools(
  service: McpToolRegistrar,
  store: McpToolCaller,
  connection: NativeMcpConnection,
): Promise<NativeMcpToolDescriptor[]> {
  for (const tool of connection.tools) {
    const remoteName = tool.remoteName;
    const handle = tool.handle;
    await service.registerTool(
      {
        name: tool.name,
        version: tool.version,
        effect: tool.effect,
        capability: tool.capability,
        description: tool.description,
        inputSchema: tool.inputSchema,
      },
      (input: unknown, request) =>
        store.mcpCallTool(
          connection.alias,
          remoteName,
          input,
          handle,
          request?.signal,
        ),
    );
  }
  return connection.tools;
}
