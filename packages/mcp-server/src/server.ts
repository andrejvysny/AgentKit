import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { WebStandardStreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/webStandardStreamableHttp.js";
import {
  CallToolRequestSchema,
  ErrorCode,
  ListToolsRequestSchema,
  McpError,
  isInitializeRequest,
  type CallToolResult,
  type ListToolsResult,
} from "@modelcontextprotocol/sdk/types.js";
import { defaultClock, type ToolCatalogEntry } from "@agentkit/host";
import { authFingerprint, resolveAuth, timingSafeEqualString } from "./auth.js";
import { checkRebindingGuard } from "./guard.js";
import {
  projectEnvelope,
  projectToolDefinition,
  visibleEntries,
} from "./projection.js";
import {
  DEFAULT_ALLOWED_HOSTS,
  DEFAULT_MAX_BATCH_SIZE,
  DEFAULT_MAX_CONCURRENT_CALLS_PER_SESSION,
  DEFAULT_MAX_REQUEST_BYTES,
  DEFAULT_MAX_SESSIONS,
  DEFAULT_SERVER_INFO,
  DEFAULT_SESSION_IDLE_TTL_MS,
  EXEC_FAILED_TEXT,
  GLOBAL_SESSION_CAP_FACTOR,
  type McpServerHandler,
  type McpServerHandlerOptions,
  type McpSessionScope,
} from "./types.js";

/**
 * The mutable half of a session: what the tool handlers write and the
 * housekeeping reads.
 *
 * Separate from {@link SessionEntry} because of WHEN each exists. The handlers
 * are built (and close over their state) before the transport has minted a
 * session id, so anything they update has to be an object that already exists
 * at `buildServer` time.
 */
interface SessionRuntime {
  /** Epoch ms of the last request ARRIVAL or COMPLETION on this session. */
  lastUsedAt: number;
  /**
   * Tool handlers currently inside this session, queued ones included.
   *
   * Reaping and eviction skip a session with any: closing one mid-`tools/call`
   * ends the SSE stream the answer was going to be written to, and the caller
   * sees HTTP 200 with an empty body — the most ambiguous outcome available for
   * a write that may well have happened.
   */
  inFlight: number;
  /** `tools/call` permits in use, and the handlers waiting for one. */
  running: number;
  waiters: (() => void)[];
  /** One shared catalogue staging for every handler that wants one right now. */
  listing: Promise<ToolCatalogEntry[]> | undefined;
}

/** One live MCP client: its transport, its server, and the scope it is pinned to. */
interface SessionEntry {
  server: Server;
  transport: WebStandardStreamableHTTPServerTransport;
  scope: McpSessionScope | undefined;
  /**
   * The principal this session belongs to — see {@link authFingerprint}. Fixed
   * at initialize; every later request must present the same one.
   */
  fingerprint: string;
  runtime: SessionRuntime;
}

/**
 * Expose an AgentKit host's tools as an MCP server, as one fetch handler.
 *
 * Mount `handler.fetch` at a path (`/mcp` by convention) on whatever server the
 * host already runs — `Bun.serve`, `Deno.serve`, a Node adapter, a Worker. The
 * package brings the protocol (the official `@modelcontextprotocol/sdk`'s
 * web-standard streamable-HTTP transport) and the policy; it never opens a
 * socket, because a transport that started owning ports and TLS would stop
 * being optional.
 *
 * ORDER OF OPERATIONS, which is the security design:
 *
 *  1. **Auth first, before anything else is read.** No session lookup, no body
 *     parse, no tool enumeration happens for a request that has not proved it
 *     may be here. A 401 costs one hash comparison and carries no body, so a
 *     caller learns "no" and nothing else.
 *  2. **The DNS-rebinding guard, before the SDK sees the request.** `Host` is
 *     checked against `allowedHosts` (loopback on any port by default), `Origin`
 *     against `allowedOrigins` when the host configured them. The SDK transport
 *     has its own version of this, deprecated in favour of exactly this: a
 *     middleware in front.
 *  3. **Session routing.** `mcp-session-id` selects a session; its absence is
 *     only legal on an `initialize` POST. Anything else is refused — a client
 *     cannot slip a `tools/call` in without first initializing, and it cannot
 *     name a session that does not exist.
 *  4. **Scope, resolved once, at init.** `sessionScope(headers)` runs on the
 *     initialize request and its answer is closed over for the session's whole
 *     life. The client's announced `clientInfo.name` is NOT an input to it, and
 *     no message body can change it afterwards: a scope a caller can restate per
 *     call is a scope a caller can borrow.
 *  5. **The session belongs to the principal that opened it.** A fingerprint
 *     of the `Authorization` header is taken at initialize and re-checked, in
 *     constant time, on every later request. A leaked `Mcp-Session-Id` alone
 *     therefore buys nothing: a caller holding a different (still valid) token
 *     gets the same 404 an invented id gets, on GET, POST and DELETE alike.
 *  6. **Write filtering, on BOTH paths.** With `writesEnabled` false (the
 *     default), `effect: "write"` tools are absent from `tools/list` AND refused
 *     by `tools/call`. Hiding alone would only stop a client that had not looked
 *     before.
 */
export function createMcpServerHandler(
  options: McpServerHandlerOptions,
): McpServerHandler {
  const verify = resolveAuth(options.auth);
  const tools = options.tools;
  const writesEnabled = options.writesEnabled ?? false;
  const allowedHosts = options.allowedHosts ?? [...DEFAULT_ALLOWED_HOSTS];
  const allowedOrigins = options.allowedOrigins;
  const serverInfo = options.serverInfo ?? { ...DEFAULT_SERVER_INFO };
  const maxSessions = options.maxSessions ?? DEFAULT_MAX_SESSIONS;
  const globalMaxSessions = maxSessions * GLOBAL_SESSION_CAP_FACTOR;
  const sessionIdleTtlMs =
    options.sessionIdleTtlMs ?? DEFAULT_SESSION_IDLE_TTL_MS;
  const maxRequestBytes = options.maxRequestBytes ?? DEFAULT_MAX_REQUEST_BYTES;
  const maxConcurrentCalls =
    options.maxConcurrentCallsPerSession ??
    DEFAULT_MAX_CONCURRENT_CALLS_PER_SESSION;
  const maxBatchSize = options.maxBatchSize ?? DEFAULT_MAX_BATCH_SIZE;
  const clock = options.clock ?? defaultClock;
  const logger = options.logger;

  const sessions = new Map<string, SessionEntry>();
  let disposed = false;

  /**
   * Build one session's `Server`, with both tool handlers closed over the scope
   * resolved for it. A fresh `Server` per session (rather than one shared across
   * transports) is what makes the scope a property of the session instead of a
   * variable the handlers have to look up and could look up wrong.
   */
  function buildServer(
    scope: McpSessionScope | undefined,
    runtime: SessionRuntime,
  ): Server {
    const server = new Server(serverInfo, {
      capabilities: { tools: { listChanged: false } },
    });

    server.setRequestHandler(
      ListToolsRequestSchema,
      async (): Promise<ListToolsResult> => {
        runtime.inFlight += 1;
        try {
          const entries = await listCatalog(runtime, scope);
          return {
            tools: visibleEntries(entries, writesEnabled).map(
              projectToolDefinition,
            ),
          };
        } finally {
          finishRequest(runtime);
        }
      },
    );

    server.setRequestHandler(
      CallToolRequestSchema,
      async (request): Promise<CallToolResult> => {
        // Counted BEFORE the queue wait, not after it: a session whose calls are
        // all still waiting for a permit is as busy as one executing them, and
        // reaping it would drop answers nobody has produced yet.
        runtime.inFlight += 1;
        try {
          await acquireCallSlot(runtime);
          try {
            return await callTool(request.params, scope, runtime);
          } finally {
            releaseCallSlot(runtime);
          }
        } finally {
          finishRequest(runtime);
        }
      },
    );

    return server;
  }

  /** The body of one `tools/call`, once it holds a concurrency permit. */
  async function callTool(
    params: { name: string; arguments?: Record<string, unknown> },
    scope: McpSessionScope | undefined,
    runtime: SessionRuntime,
  ): Promise<CallToolResult> {
    const name = params.name;
    const entries = visibleEntries(
      await listCatalog(runtime, scope),
      writesEnabled,
    );
    const entry = entries.find((it) => it.definition.name === name);
    if (entry === undefined) {
      // A hidden write tool and a tool that never existed answer the SAME
      // way. Distinguishing them would confirm the name to a client that is
      // not allowed to call it.
      //
      // Thrown, not returned: an unknown method parameter is a PROTOCOL
      // error (a JSON-RPC error response the SDK builds from this), not a
      // tool that ran and failed. Reporting it as `isError` content would
      // tell the model its call was dispatched.
      throw new McpError(ErrorCode.InvalidParams, `Unknown tool: ${name}`);
    }

    try {
      const envelope = await tools.execute(name, params.arguments ?? {}, scope);
      return projectEnvelope(envelope);
    } catch (err) {
      if (err instanceof McpError) throw err;
      // A source that THREW is a fault in host code, and the call is what
      // failed — not the protocol. Report it as a failed tool result so the
      // session survives and the caller learns something happened.
      //
      // Not WHY, though: the thrower is host code, and its message is written
      // for an operator's log, not for a remote MCP client that has just been
      // told the host's internals. The client gets a correlation id; the
      // operator gets the message under the same id.
      const errorMessage = err instanceof Error ? err.message : String(err);
      const correlationId = crypto.randomUUID().slice(0, 8);
      logger?.error("mcp tool source threw", {
        tool: name,
        correlationId,
        errorMessage,
      });
      return {
        isError: true,
        content: [
          {
            type: "text",
            text: JSON.stringify({
              errorCode: "exec_failed",
              errorMessage: `${EXEC_FAILED_TEXT} (ref: ${correlationId})`,
              phase: "execution",
            }),
          },
        ],
      };
    }
  }

  /**
   * The session's tool catalogue, computed ONCE for everyone asking right now.
   *
   * A JSON-RPC batch is dispatched message-by-message without waiting, so all
   * of its calls reach this in the same tick — and each of them staging the
   * whole registry (with an Ajv compile per tool) is the cost the batch cap and
   * this share exist to bound. What they share is a listing for ONE session, so
   * the same scope and the same guards would have answered each of them the
   * same way; nothing is cached ACROSS requests, and `tools.execute` re-stages
   * per call regardless, so a `canExecute` guard is still evaluated at call
   * time on state that may have moved.
   */
  function listCatalog(
    runtime: SessionRuntime,
    scope: McpSessionScope | undefined,
  ): Promise<ToolCatalogEntry[]> {
    const inFlight = runtime.listing;
    if (inFlight) return inFlight;
    const started = tools.catalog.listTools(scope);
    const tracked = started.finally(() => {
      if (runtime.listing === tracked) runtime.listing = undefined;
    });
    runtime.listing = tracked;
    return tracked;
  }

  /** One permit, or a place in the queue for the next one released. */
  function acquireCallSlot(runtime: SessionRuntime): Promise<void> {
    if (runtime.running < maxConcurrentCalls) {
      runtime.running += 1;
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      runtime.waiters.push(resolve);
    });
  }

  /** Hand the permit to the next waiter, or give it back to the pool. */
  function releaseCallSlot(runtime: SessionRuntime): void {
    const next = runtime.waiters.shift();
    if (next === undefined) {
      runtime.running -= 1;
      return;
    }
    next();
  }

  /**
   * A handler left the session: it is one less reason not to reap, and the
   * session was in use as recently as NOW — not as recently as when the request
   * arrived, which for a long tool call is far enough in the past to be reaped
   * out from under the answer.
   */
  function finishRequest(runtime: SessionRuntime): void {
    runtime.inFlight -= 1;
    runtime.lastUsedAt = clock.now().getTime();
  }

  /**
   * FAIL CLOSED. A host-supplied `verify` that throws has not authenticated
   * anybody, so the answer is the same 401 a wrong token gets — the alternative
   * is a rejected promise the surrounding server turns into a 500, on a path
   * whose whole job is to say no. It is logged at `error` so a broken verifier
   * is not indistinguishable from a wrong password in the operator's logs.
   */
  async function authorized(request: Request): Promise<boolean> {
    try {
      return await verify(request.headers.get("authorization"));
    } catch (err) {
      logger?.error("mcp auth verifier threw; refusing the request", {
        error: err instanceof Error ? err.message : String(err),
      });
      return false;
    }
  }

  /**
   * Drop one session and end everything it holds open.
   *
   * `Server.close()` closes the transport under it, which closes every SSE
   * stream that session's client is still reading — the map entry alone is not
   * what keeps a dead session expensive. Fire-and-forget because the callers
   * are housekeeping on somebody else's request: a slow or throwing close must
   * not delay, or fail, the request that happened to trigger the sweep.
   */
  function closeSession(
    sessionId: string,
    entry: SessionEntry,
    reason: "expired" | "evicted",
  ): void {
    sessions.delete(sessionId);
    logger?.debug("mcp session closed", { sessionId, reason });
    void entry.server.close().catch((err) => {
      logger?.warn("mcp session failed to close", {
        sessionId,
        reason,
        error: err instanceof Error ? err.message : String(err),
      });
    });
  }

  /**
   * Close every session idle longer than `sessionIdleTtlMs`.
   *
   * LAZY, on the request path: no timer is ever armed, so mounting this handler
   * does not keep an event loop alive. The cost is that a stale session lingers
   * until the next request arrives — which is when it starts mattering.
   */
  function reapIdleSessions(): void {
    const cutoff = clock.now().getTime() - sessionIdleTtlMs;
    for (const [sessionId, entry] of sessions) {
      if (entry.runtime.inFlight > 0) continue;
      if (entry.runtime.lastUsedAt < cutoff) {
        closeSession(sessionId, entry, "expired");
      }
    }
  }

  /**
   * Make room for one more session by closing the least recently used ones.
   *
   * Evicting the OLDEST IDLE rather than refusing the newcomer: a client that
   * walked away holding a session must not be able to lock a live one out, and
   * nothing in MCP obliges a client to send the DELETE that would free it.
   *
   * Two caps, in this order. The per-principal one first, so a caller opening
   * sessions can only ever displace its own; the global one after, as a
   * backstop for a host that mints a token per client — only there does
   * eviction cross principals, and only once the map is
   * {@link GLOBAL_SESSION_CAP_FACTOR} times over.
   */
  function evictForCapacity(fingerprint: string): void {
    evictOldest(maxSessions, (entry) => entry.fingerprint === fingerprint);
    evictOldest(globalMaxSessions, () => true);
  }

  /**
   * Close the oldest idle sessions matching `matches` until fewer than `cap`
   * remain.
   *
   * A session with a request in flight is never a victim (see
   * {@link SessionRuntime.inFlight}), so the loop stops when every candidate is
   * busy — going one over the cap is the lesser fault next to answering a live
   * `tools/call` with an empty body. Counting by scan rather than by a second
   * index keyed on fingerprint: the caps are small, and a map that has to be
   * kept in step with `sessions` is a map that can fall out of step with it.
   */
  function evictOldest(
    cap: number,
    matches: (entry: SessionEntry) => boolean,
  ): void {
    for (;;) {
      let count = 0;
      let oldestId: string | undefined;
      let oldestUsedAt = Number.POSITIVE_INFINITY;
      for (const [sessionId, entry] of sessions) {
        if (!matches(entry)) continue;
        count += 1;
        if (entry.runtime.inFlight > 0) continue;
        if (entry.runtime.lastUsedAt < oldestUsedAt) {
          oldestUsedAt = entry.runtime.lastUsedAt;
          oldestId = sessionId;
        }
      }
      if (count < cap) return;
      if (oldestId === undefined) {
        // Only interesting when there WAS something to evict: a cap of 0 has no
        // candidates and needs no warning about it.
        if (count > 0) {
          logger?.warn("mcp session cap reached with every session busy", {
            cap,
            live: count,
          });
        }
        return;
      }
      const victim = sessions.get(oldestId);
      if (victim === undefined) return;
      closeSession(oldestId, victim, "evicted");
    }
  }

  async function openSession(headers: Headers): Promise<SessionEntry> {
    // Taken from the request that INITIALIZES the session, before the session
    // exists, for the same reason the scope is: it is what the caller proved
    // with, not something a later message can restate.
    const fingerprint = await authFingerprint(headers.get("authorization"));
    const scope = (await options.sessionScope?.(headers)) ?? undefined;
    const runtime: SessionRuntime = {
      lastUsedAt: clock.now().getTime(),
      inFlight: 0,
      running: 0,
      waiters: [],
      listing: undefined,
    };
    const server = buildServer(scope, runtime);
    const transport = new WebStandardStreamableHTTPServerTransport({
      sessionIdGenerator: () => crypto.randomUUID(),
      onsessioninitialized: (sessionId) => {
        // `dispose()` can land while this initialize was in flight (resolving
        // the scope, connecting). Registering now would leak a live session
        // past a shutdown that already swept the map.
        if (disposed) {
          void server.close();
          return;
        }
        // At the moment of insertion, not before it: two initializes racing
        // each other would both pass a check made earlier and leave the map one
        // over the cap.
        evictForCapacity(fingerprint);
        sessions.set(sessionId, entry);
        logger?.debug("mcp session opened", { sessionId });
      },
      onsessionclosed: (sessionId) => {
        sessions.delete(sessionId);
        logger?.debug("mcp session closed", { sessionId });
      },
    });
    const entry: SessionEntry = {
      server,
      transport,
      scope,
      fingerprint,
      runtime,
    };
    // A transport that dies for any other reason (stream error, dispose) must
    // not leave a dangling map entry that later requests would route into.
    transport.onclose = () => {
      const sessionId = transport.sessionId;
      if (sessionId !== undefined) sessions.delete(sessionId);
    };
    await server.connect(transport);
    return entry;
  }

  return {
    async fetch(request: Request): Promise<Response> {
      if (disposed) {
        return jsonRpcError(503, -32000, "Server is shutting down");
      }

      if (!(await authorized(request))) {
        // No body, no hint about which half was wrong.
        return new Response(null, {
          status: 401,
          headers: { "WWW-Authenticate": 'Bearer realm="agentkit-mcp"' },
        });
      }

      const refusal = checkRebindingGuard(request.headers, {
        allowedHosts,
        ...(allowedOrigins === undefined ? {} : { allowedOrigins }),
      });
      if (refusal !== null) {
        logger?.warn("mcp request refused by rebinding guard", {
          reason: refusal,
        });
        return new Response(null, { status: 403 });
      }

      // Housekeeping runs after auth, on a request that has already proved it
      // may be here, and before any session lookup — so an expired session is
      // gone by the time it could be routed into.
      reapIdleSessions();

      // The body is read HERE, under a cap, for EVERY post — and handed back to
      // the transport as `parsedBody`, because a `Request` body is single-use.
      // Two reasons it cannot be left to the SDK: the transport buffers the
      // whole thing before any limit of ours could apply, and a batch has to be
      // measured before one message of it is dispatched.
      let body: unknown;
      if (request.method === "POST") {
        const read = await readCappedJson(request, maxRequestBytes);
        if (!read.ok) return read.response;
        body = read.body;
        if (Array.isArray(body) && body.length > maxBatchSize) {
          logger?.warn("mcp batch refused", {
            messages: body.length,
            maxBatchSize,
          });
          return jsonRpcError(
            400,
            -32600,
            `Invalid Request: batch of ${body.length} messages exceeds the ` +
              `limit of ${maxBatchSize}`,
          );
        }
      }

      const sessionId = request.headers.get("mcp-session-id");
      if (sessionId !== null) {
        const entry = sessions.get(sessionId);
        if (entry === undefined) {
          return jsonRpcError(404, -32001, "Session not found");
        }
        // A session id is a routing key, not a credential. Being authenticated
        // is not enough: this must be the SAME principal that opened it, or a
        // leaked id would hand one caller another's pinned scope — and, on
        // DELETE, another's session. The refusal is byte-identical to the
        // unknown-id one above: confirming that the session exists but is not
        // yours is confirming that it exists.
        const presented = await authFingerprint(
          request.headers.get("authorization"),
        );
        if (!(await timingSafeEqualString(entry.fingerprint, presented))) {
          return jsonRpcError(404, -32001, "Session not found");
        }
        entry.runtime.lastUsedAt = clock.now().getTime();
        return request.method === "POST"
          ? entry.transport.handleRequest(request, { parsedBody: body })
          : entry.transport.handleRequest(request);
      }

      if (request.method !== "POST") {
        return jsonRpcError(
          400,
          -32000,
          "Bad Request: Mcp-Session-Id header is required",
        );
      }

      // `isInitializeRequest` is checked before a session is created: a client
      // cannot slip any other method in without one.
      if (!isInitializeRequest(body)) {
        return jsonRpcError(
          400,
          -32000,
          "Bad Request: Mcp-Session-Id header is required",
        );
      }

      const entry = await openSession(request.headers);
      return entry.transport.handleRequest(request, { parsedBody: body });
    },

    async dispose(): Promise<void> {
      disposed = true;
      const open = [...sessions.values()];
      sessions.clear();
      for (const entry of open) {
        // `Server.close()` closes the transport it is connected to, which ends
        // every SSE stream that session holds open. A throw from one session
        // must not strand the rest of the shutdown.
        try {
          await entry.server.close();
        } catch (err) {
          logger?.warn("mcp session failed to close", {
            error: err instanceof Error ? err.message : String(err),
          });
        }
      }
    },
  };
}

/** The JSON-RPC error envelope the streamable-HTTP transport itself uses. */
function jsonRpcError(status: number, code: number, message: string): Response {
  return new Response(
    JSON.stringify({ jsonrpc: "2.0", error: { code, message }, id: null }),
    { status, headers: { "Content-Type": "application/json" } },
  );
}

/** Either the parsed body, or the response that refuses it. */
type BodyRead = { ok: true; body: unknown } | { ok: false; response: Response };

/**
 * Read a POST body under a byte cap and parse it as JSON.
 *
 * `Content-Length` is checked first, so an oversized request that declares
 * itself is refused without reading a byte; the read then counts anyway,
 * because a chunked body declares nothing and a lying header is not a
 * constraint. The cap is on the ENCODED bytes, which is what the peer sends and
 * what memory pays for.
 */
async function readCappedJson(
  request: Request,
  maxBytes: number,
): Promise<BodyRead> {
  const declared = Number(request.headers.get("content-length") ?? "");
  if (Number.isFinite(declared) && declared > maxBytes) {
    return { ok: false, response: tooLarge(maxBytes) };
  }
  const text = await readCappedText(request.body, maxBytes);
  if (text === null) return { ok: false, response: tooLarge(maxBytes) };
  try {
    return { ok: true, body: JSON.parse(text) };
  } catch {
    return { ok: false, response: jsonRpcError(400, -32700, "Parse error") };
  }
}

/** The decoded body, or `null` once more than `maxBytes` have arrived. */
async function readCappedText(
  body: ReadableStream<Uint8Array> | null,
  maxBytes: number,
): Promise<string | null> {
  if (body === null) return "";
  const reader = body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const chunk = await reader.read();
    if (chunk.done) break;
    total += chunk.value.byteLength;
    if (total > maxBytes) {
      // Cancelled, not just abandoned: the point of the cap is that the rest of
      // the body is never buffered.
      await reader.cancel();
      return null;
    }
    chunks.push(chunk.value);
  }
  const joined = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    joined.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(joined);
}

function tooLarge(maxBytes: number): Response {
  return jsonRpcError(
    413,
    -32000,
    `Request body exceeds the ${maxBytes}-byte limit`,
  );
}
