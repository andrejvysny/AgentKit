import type { AiToolEnvelope } from "@agentkit/contracts";
import type { Logger, ToolCatalog } from "@agentkit/host";

/**
 * The scope one MCP session works in.
 *
 * Resolved ONCE, from the transport headers of the request that initialized the
 * session (see {@link McpServerHandlerOptions.sessionScope}), and never from
 * anything the client puts in a message body: a `chatId` a client can name per
 * call is a `chatId` a client can name someone else's.
 */
export interface McpSessionScope {
  chatId?: string;
}

/**
 * The two halves of "an AgentKit host's tools", as an MCP server needs them.
 *
 * They are one object rather than two options because they must agree: what
 * {@link catalog} advertises is exactly what {@link execute} is allowed to run,
 * and a host that wires a catalogue from one contributor set and an executor
 * over another has built a server that lists one thing and runs another.
 * `createStagedToolSource` builds both from a single contributor list for that
 * reason.
 *
 * `catalog` is `@agentkit/host`'s {@link ToolCatalog} verbatim — definitions
 * only, no executables — so the projection layer cannot accidentally acquire a
 * second, unguarded call path (see `packages/host/src/ports/tool-catalog.ts`).
 */
export interface McpToolSource {
  catalog: ToolCatalog;
  /**
   * Run one tool and return the model-facing envelope.
   *
   * `scope` is the session's, never the caller's: the handler passes what
   * `sessionScope` resolved at session init.
   *
   * A tool that FAILS returns an `ok: false` envelope; a thrown error is a
   * source-level fault (the tool vanished between listing and calling, the
   * store is down) and the handler reports it as a failed call rather than
   * letting it kill the session.
   */
  execute(
    name: string,
    args: unknown,
    scope?: McpSessionScope,
  ): Promise<AiToolEnvelope>;
}

/**
 * How a request proves it is allowed to talk to this server.
 *
 * `bearerToken` is the batteries-included form: the handler does the
 * `Authorization: Bearer …` parse and a constant-time comparison for you (see
 * `auth.ts`). `verify` is the escape hatch for a host that mints per-client
 * tokens, checks a JWT, or asks an external service — it is handed the raw
 * header (or `null` when there is none) and answers yes/no. There is no
 * "unauthenticated" variant on purpose: this server hands out tool execution.
 */
export type McpServerAuth =
  | { bearerToken: string }
  | { verify(authorizationHeader: string | null): boolean | Promise<boolean> };

export interface McpServerHandlerOptions {
  tools: McpToolSource;
  auth: McpServerAuth;
  /**
   * Exact origins (`scheme://host[:port]`) a browser-originated request may
   * carry. Omitted, the `Origin` header is not checked — the common client is a
   * native MCP host that sends none, and the `Host` guard below is the one that
   * actually stops a DNS-rebinding attack. Given, a request that carries an
   * `Origin` outside the list is refused; a request with no `Origin` still
   * passes, because absence is not a claim.
   */
  allowedOrigins?: string[];
  /**
   * Host header values this server answers to. Defaults to
   * {@link DEFAULT_ALLOWED_HOSTS} — `localhost`, `127.0.0.1` and `[::1]` on ANY
   * port. An entry without a port matches any port on that hostname; an entry
   * with one must match exactly.
   */
  allowedHosts?: string[];
  /** Advertised to clients in the `initialize` result. */
  serverInfo?: { name: string; version: string };
  /**
   * Whether tools declaring `effect: "write"` are exposed. **Default `false`.**
   * A write reached over MCP skips the run loop's proposal pipeline and the UI
   * that would show it, so it is opt-in per server, not per call.
   */
  writesEnabled?: boolean;
  /**
   * Resolve the scope for a NEW session from that request's headers.
   *
   * Called exactly once per session, on the `initialize` request, before the
   * session exists. Everything the client sends afterwards runs in the scope
   * this returned — including the `clientInfo.name` it announced, which is
   * deliberately not an input here: it is a self-reported string, and keying
   * authority on it lets any client claim any other client's scope.
   */
  sessionScope?(
    headers: Headers,
  ): McpSessionScope | Promise<McpSessionScope> | undefined;
  logger?: Logger;
}

/** What {@link createMcpServerHandler} hands back. */
export interface McpServerHandler {
  /** Mount this under whatever path the host serves MCP on (`/mcp` by convention). */
  fetch(request: Request): Promise<Response>;
  /**
   * Close every live session. Idempotent; after it resolves the handler answers
   * 503 rather than opening new sessions.
   */
  dispose(): Promise<void>;
}

/** Loopback only, any port — see {@link McpServerHandlerOptions.allowedHosts}. */
export const DEFAULT_ALLOWED_HOSTS: readonly string[] = Object.freeze([
  "localhost",
  "127.0.0.1",
  "[::1]",
]);

/** What the server calls itself when the host does not say. */
export const DEFAULT_SERVER_INFO = {
  name: "agentkit",
  version: "0.1.0",
} as const;
