import type { AiToolEnvelope } from "@agentkit/contracts";
import type { Clock, Logger, ToolCatalog } from "@agentkit/host";

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
  /**
   * Who this session belongs to, as the HOST names principals.
   *
   * Resolved by {@link McpServerHandlerOptions.sessionScope} from the same
   * headers the `chatId` comes from, and threaded to the tools that run for the
   * session: `AiToolExecutionContext.metadata.principal` and
   * `ToolGuardContext.principal`. It is an opaque label for policy and audit —
   * the handler never compares it to anything, because the thing that decides
   * whether a request may use a session is the `Authorization` fingerprint (see
   * `auth.ts`), not a string the scope callback derived.
   */
  principal?: string;
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
  /**
   * How many sessions ONE PRINCIPAL may hold at once. Defaults to
   * {@link DEFAULT_MAX_SESSIONS} (64).
   *
   * A session holds a `Server`, a transport, and every SSE stream the client
   * opened on it, and nothing about the protocol obliges a client to ever send
   * the DELETE that ends one — so an unbounded map is a memory leak any
   * authenticated caller can drive by reconnecting. At the cap, the session
   * that has gone longest without a request is closed to make room: the
   * alternative, refusing the new one, lets a stale session lock a live client
   * out.
   *
   * PER PRINCIPAL, not global (the principal being the `Authorization`
   * fingerprint the session was opened with). A global LRU means one caller
   * opening `maxSessions` sessions closes everybody else's — a cross-tenant
   * denial of service anyone holding a valid token can run. A principal can
   * only ever evict its own. The whole map is still bounded, by
   * `maxSessions * `{@link GLOBAL_SESSION_CAP_FACTOR}, as a backstop against a
   * host that mints a token per client; only past THAT does eviction cross
   * principals.
   */
  maxSessions?: number;
  /**
   * How long a session may go without a request before it is closed. Defaults
   * to {@link DEFAULT_SESSION_IDLE_TTL_MS} (30 minutes).
   *
   * Reaped LAZILY — on the next request the handler serves, not on a timer. A
   * handler that armed an interval would keep an event loop alive for as long
   * as the process runs, in a package whose whole shape is "a function that
   * takes a `Request`"; a host that wants eager cleanup calls `dispose()`.
   */
  sessionIdleTtlMs?: number;
  /**
   * Largest POST body this handler will read, in bytes. Defaults to
   * {@link DEFAULT_MAX_REQUEST_BYTES} (4 MiB); a bigger one is refused `413`.
   *
   * The check is made twice on purpose: a `Content-Length` over the cap is
   * refused before a single byte is read, and the body is then read through a
   * counter that aborts at the cap — because a chunked request declares no
   * length, and the SDK transport buffers whatever arrives before a session (or
   * a message) exists to attribute it to.
   */
  maxRequestBytes?: number;
  /**
   * How many `tools/call` requests ONE session may have executing at once.
   * Defaults to {@link DEFAULT_MAX_CONCURRENT_CALLS_PER_SESSION} (4); the rest
   * queue.
   *
   * A JSON-RPC batch is dispatched message-by-message with no waiting in
   * between, so without this one request can put its whole batch into the host's
   * tools simultaneously — every one of them staging a registry and running a
   * tool against the host's own state.
   */
  maxConcurrentCallsPerSession?: number;
  /**
   * How many messages one JSON-RPC batch may carry. Defaults to
   * {@link DEFAULT_MAX_BATCH_SIZE} (8); a longer array is refused `-32600`
   * before anything in it is dispatched.
   *
   * The cap is on the array, not on the work it implies: refusing after the
   * first few messages have already been handed to the transport would run
   * exactly the tools the limit exists to bound.
   */
  maxBatchSize?: number;
  /**
   * Source of the timestamps {@link maxSessions} and {@link sessionIdleTtlMs}
   * compare. Defaults to `@agentkit/host`'s `defaultClock`; injected in tests
   * so an idle-TTL assertion is about a fake clock rather than about waiting.
   */
  clock?: Clock;
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

/** Per-principal session cap — see {@link McpServerHandlerOptions.maxSessions}. */
export const DEFAULT_MAX_SESSIONS = 64;

/**
 * Multiple of `maxSessions` at which eviction stops respecting principals.
 *
 * The per-principal cap bounds what one caller can hold; this bounds what ALL
 * of them can, for a host whose `verify` accepts a token per client. Reaching
 * it means the map is already `maxSessions * 16` entries deep, so the
 * globally-oldest session is closed regardless of whose it is.
 */
export const GLOBAL_SESSION_CAP_FACTOR = 16;

/** Request body cap — see {@link McpServerHandlerOptions.maxRequestBytes}. */
export const DEFAULT_MAX_REQUEST_BYTES = 4 * 1024 * 1024;

/**
 * Concurrent `tools/call` cap per session — see
 * {@link McpServerHandlerOptions.maxConcurrentCallsPerSession}.
 */
export const DEFAULT_MAX_CONCURRENT_CALLS_PER_SESSION = 4;

/** JSON-RPC batch cap — see {@link McpServerHandlerOptions.maxBatchSize}. */
export const DEFAULT_MAX_BATCH_SIZE = 8;

/**
 * Idle session lifetime — see
 * {@link McpServerHandlerOptions.sessionIdleTtlMs}.
 */
export const DEFAULT_SESSION_IDLE_TTL_MS = 30 * 60 * 1000;

/**
 * What a tool that THREW tells the MCP client, in place of the thrown message.
 *
 * A thrower's message is written for the host's own log — it names paths,
 * queries, rows — and an MCP client is a remote caller, often the model itself.
 * It gets this sentence plus a correlation id (`runId` on the staged path, a
 * random short id when the source itself threw); the operator gets the real
 * message logged under the same id.
 */
export const EXEC_FAILED_TEXT = "The host failed to execute the tool";

/** What the server calls itself when the host does not say. */
export const DEFAULT_SERVER_INFO = {
  name: "agentkit",
  version: "0.1.0",
} as const;
