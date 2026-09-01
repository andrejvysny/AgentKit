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
import { defaultClock } from "@agentkit/host";
import { authFingerprint, resolveAuth, timingSafeEqualString } from "./auth.js";
import { checkRebindingGuard } from "./guard.js";
import {
  projectEnvelope,
  projectToolDefinition,
  visibleEntries,
} from "./projection.js";
import {
  DEFAULT_ALLOWED_HOSTS,
  DEFAULT_MAX_SESSIONS,
  DEFAULT_SERVER_INFO,
  DEFAULT_SESSION_IDLE_TTL_MS,
  type McpServerHandler,
  type McpServerHandlerOptions,
  type McpSessionScope,
} from "./types.js";

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
  /** Epoch ms of the last request served on this session; drives both the
   * idle TTL and the eviction order. */
  lastUsedAt: number;
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
  const sessionIdleTtlMs =
    options.sessionIdleTtlMs ?? DEFAULT_SESSION_IDLE_TTL_MS;
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
  function buildServer(scope: McpSessionScope | undefined): Server {
    const server = new Server(serverInfo, {
      capabilities: { tools: { listChanged: false } },
    });

    server.setRequestHandler(
      ListToolsRequestSchema,
      async (): Promise<ListToolsResult> => {
        const entries = await tools.catalog.listTools(scope);
        return {
          tools: visibleEntries(entries, writesEnabled).map(
            projectToolDefinition,
          ),
        };
      },
    );

    server.setRequestHandler(
      CallToolRequestSchema,
      async (request): Promise<CallToolResult> => {
        const name = request.params.name;
        const entries = visibleEntries(
          await tools.catalog.listTools(scope),
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
          const envelope = await tools.execute(
            name,
            request.params.arguments ?? {},
            scope,
          );
          return projectEnvelope(envelope);
        } catch (err) {
          if (err instanceof McpError) throw err;
          // A source that THREW is a fault in host code, and the call is what
          // failed — not the protocol. Report it as a failed tool result so the
          // session survives and the caller learns why.
          const errorMessage = err instanceof Error ? err.message : String(err);
          logger?.error("mcp tool source threw", { tool: name, errorMessage });
          return {
            isError: true,
            content: [
              {
                type: "text",
                text: JSON.stringify({
                  errorCode: "exec_failed",
                  errorMessage,
                  phase: "execution",
                }),
              },
            ],
          };
        }
      },
    );

    return server;
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
      if (entry.lastUsedAt < cutoff) closeSession(sessionId, entry, "expired");
    }
  }

  /**
   * Make room for one more session by closing the least recently used ones.
   *
   * Evicting the OLDEST IDLE rather than refusing the newcomer: a client that
   * walked away holding a session must not be able to lock a live one out, and
   * nothing in MCP obliges a client to send the DELETE that would free it. The
   * loop is written to stop when the map is empty so a `maxSessions` of 0
   * cannot spin.
   */
  function evictForCapacity(): void {
    while (sessions.size >= maxSessions) {
      let oldestId: string | undefined;
      let oldestUsedAt = Number.POSITIVE_INFINITY;
      for (const [sessionId, entry] of sessions) {
        if (entry.lastUsedAt < oldestUsedAt) {
          oldestUsedAt = entry.lastUsedAt;
          oldestId = sessionId;
        }
      }
      if (oldestId === undefined) return;
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
    const server = buildServer(scope);
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
        evictForCapacity();
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
      lastUsedAt: clock.now().getTime(),
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
        entry.lastUsedAt = clock.now().getTime();
        return entry.transport.handleRequest(request);
      }

      if (request.method !== "POST") {
        return jsonRpcError(
          400,
          -32000,
          "Bad Request: Mcp-Session-Id header is required",
        );
      }

      // The body is read HERE, so `isInitializeRequest` can be checked before a
      // session is created — and handed back to the transport as `parsedBody`,
      // because a `Request` body is single-use.
      let body: unknown;
      try {
        body = await request.json();
      } catch {
        return jsonRpcError(400, -32700, "Parse error");
      }
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
