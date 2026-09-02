# @agentkit/mcp-server

Exposes an AgentKit host's tools **as** a [Model Context
Protocol](https://modelcontextprotocol.io) server, over streamable HTTP, behind
one fetch-standard handler. The mirror image of
[`@agentkit/mcp-client`](../mcp-client): that package brings other people's
tools into your runs, this one lets Claude Desktop, an IDE, or another agent
call yours.

Built on the official `@modelcontextprotocol/sdk` — the low-level `Server` plus
its `WebStandardStreamableHTTPServerTransport`, so no Node `IncomingMessage`
bridge and no framework: the handler takes a `Request` and returns a `Response`.
What this package adds on top is the part a host would otherwise have to get
right itself: constant-time bearer auth, a DNS-rebinding guard, a session's
scope pinned at initialize, verbatim tool projection, and write-tool filtering.

## Wiring

```ts
import { createMcpServerHandler, createStagedToolSource } from "@agentkit/mcp-server";

const mcp = createMcpServerHandler({
  // Both halves — catalogue and executor — from ONE contributor list, so what
  // is listed and what is runnable cannot drift.
  tools: createStagedToolSource({
    contributors,      // the same array the TurnRunner gets
    context,           // optional ContextProvider, for chat bindings
    guards,            // optional ToolGuards — the same ones the runner uses
    clock,
    ids,
    logger,
  }),
  auth: { bearerToken: process.env.AGENTKIT_MCP_SERVER_TOKEN! },
  // Loopback on any port by default; name your own list to widen it.
  allowedHosts: ["localhost", "127.0.0.1"],
  // Which chat an MCP session works in — and, optionally, who it belongs to —
  // from ITS OWN headers, once, at init.
  sessionScope: (headers) => {
    const chatId = headers.get("x-agentkit-chat");
    const principal = headers.get("x-agentkit-principal");
    return {
      ...(chatId === null ? {} : { chatId }),
      ...(principal === null ? {} : { principal }),
    };
  },
  writesEnabled: false, // default; see "Writes" below
  // Session lifetime — both defaults shown; see "Session lifetime" below.
  maxSessions: 64,               // PER PRINCIPAL
  sessionIdleTtlMs: 30 * 60 * 1000,
  // Request bounds — all defaults shown; see "Request bounds" below.
  maxRequestBytes: 4 * 1024 * 1024,
  maxConcurrentCallsPerSession: 4,
  maxBatchSize: 8,
  clock,                // optional; only the lifetime settings read it
  logger,
});

Bun.serve({
  hostname: "127.0.0.1",
  port: 8787,
  fetch(request) {
    if (new URL(request.url).pathname === "/mcp") return mcp.fetch(request);
    return restHandler(request);
  },
});

// On shutdown — closes every live session's SSE stream.
await mcp.dispose();
```

A client config, for a host that reads one:

```jsonc
{
  "mcpServers": {
    "agentkit": {
      "type": "http",
      "url": "http://127.0.0.1:8787/mcp",
      "headers": { "Authorization": "Bearer <AGENTKIT_MCP_SERVER_TOKEN>" }
    }
  }
}
```

## Security model

This server hands out **tool execution against the host's own state**. It is
built for a desktop host on loopback, and every default is the conservative one.

**Auth is not optional.** There is no unauthenticated mode: `auth` is either
`{ bearerToken }` or `{ verify(header) }`. A bearer token is compared in
constant time — both sides are SHA-256'd (Web Crypto, no `node:*` import) and
the two 32-byte digests are compared with a non-short-circuiting XOR, so the
comparison is always the same width and the token's **length leaks nothing**;
an early length check would answer "your guess is the wrong length", which is
the one bit worth hiding. A refusal is a `401` with **no body**: a wrong token,
a missing header and a wrong scheme are indistinguishable to the caller. An empty `bearerToken` is refused at wiring
time, not at the first request — a host that read its token out of an unset
environment variable should find that out at boot.

**Auth runs before anything else.** No session lookup, no body parse, no tool
enumeration happens for a request that has not authenticated. An unknown
session id with no credentials is `401`, not `404`: the 404 would confirm the
id does not exist.

**DNS-rebinding guard.** `Host` is checked against `allowedHosts` — default
`localhost`, `127.0.0.1`, `[::1]`, on **any** port; an entry that names a port
must match it exactly. A page on `evil.com` whose DNS is re-pointed at
`127.0.0.1` still sends `Host: evil.com`, which is why this is the load-bearing
check and why it is on by default. `Origin` is checked only when you configure
`allowedOrigins`, and only when the request carries one: the ordinary MCP client
is a native process that sends no `Origin`, so requiring one would refuse every
real client to defend against a browser that is not there. Refusals are a `403`
with no body; the reason goes to the `logger`.

**A session's scope is resolved once, at `initialize`, from that request's
headers.** `sessionScope(headers)` runs before the session exists, and the
answer is closed over for the session's whole life. Two things are deliberately
*not* inputs: anything in a message body, and the client's announced
`clientInfo.name`. A self-reported name is a name any client can claim, and a
scope a caller can restate per call is a scope a caller can borrow. Sessions
are keyed on the MCP `Mcp-Session-Id` header, which the server mints.

**A session belongs to the principal that opened it.** At `initialize` the
handler fingerprints the request's raw `Authorization` header (SHA-256; the
empty string when there is none) and stores the digest, not the header. Every
later request on that session id — GET, POST and DELETE alike — must present
the same fingerprint, compared in constant time. A caller who authenticates
with a *different* valid token and presents a leaked `Mcp-Session-Id` gets a
`404` byte-identical to the one an invented id gets: a `401`/`403` there would
confirm the session exists, and the DELETE it would otherwise be allowed to
send would end someone else's session.

**A session's principal reaches the tools.** Whatever `sessionScope` returns as
`principal` is threaded to `AiToolExecutionContext.metadata.principal` and to
`ToolGuardContext.principal`, on BOTH paths — so a guard can hide or refuse per
caller and `tools/list` shows exactly what `tools/call` would allow. It is a
label for policy and audit, never a credential: what decides whether a request
may use a session is the `Authorization` fingerprint above, not a string the
scope callback derived.

**A thrown tool does not explain itself to the client.** When a tool (or the
tool source) throws, the MCP client gets `exec_failed` with a fixed sentence and
a correlation id; the thrower's own message goes to the `logger` under the same
id. A throw's message is written for an operator — it names paths, queries, rows
that were not found — and the caller here is a remote agent.

**Bind loopback.** Even with a token, publishing this to a LAN publishes tool
execution to a LAN.

**Composition hazard: CORS.** `@agentkit/transport-http`'s `cors` option is
about the REST surface, but a browser does not know that. Serving this handler
from the same origin as a transport-http server configured with
`origins: "*"` — whose default allow-headers include `Authorization` — makes
tool execution reachable from any page the user has open, since the browser will
now be told the cross-origin request is permitted. If both are mounted on one
origin, set `allowedOrigins` here to the exact origins your UI is served from
(and prefer a named origin list over `"*"` there). The `Host` guard does not
cover this case: a real browser sends your own host.

## Session lifetime

Nothing in MCP obliges a client to send the `DELETE` that ends a session, and a
session holds a `Server`, a transport and every SSE stream its client opened —
so the map is bounded on two axes.

- **`maxSessions`** (default **64**), **per principal**. At the cap, opening a
  new session closes the one that has gone longest without a request, and closes
  its transport with it — so its open streams end rather than lingering.
  Evicting the oldest idle rather than refusing the newcomer is deliberate: a
  client that walked away must not be able to lock a live one out. The bucket is
  the `Authorization` fingerprint, so a caller can only ever evict its **own**
  sessions; a global LRU would let anyone holding a valid token close everybody
  else's by reconnecting. The map as a whole is still bounded, at
  `maxSessions × 16`, and only past that does eviction cross principals.
- **`sessionIdleTtlMs`** (default **30 minutes**). A session idle longer than
  this is closed. Reaped **lazily**, on the next request the handler serves —
  no timer is armed, because a package whose whole shape is "a function that
  takes a `Request`" should not keep an event loop alive. A host that wants
  eager cleanup calls `dispose()`.

Neither reaping nor eviction touches a session with a **request in flight**, and
a session's idle clock is stamped when a request COMPLETES as well as when it
arrives. Closing a session mid-`tools/call` ends the SSE stream the answer was
going to be written to, and the caller gets HTTP 200 with an empty body — the
most ambiguous outcome available for a call that may well have run.

Both compare timestamps from `clock` (default `@agentkit/host`'s
`defaultClock`), which is injectable so an idle-TTL test is about a fake clock
rather than about waiting. An evicted or expired session id answers `404`, the
same as an unknown one.

## Request bounds

This handler is reachable by anything holding the token, and every one of these
is a resource an authenticated caller could otherwise spend without limit.

- **`maxRequestBytes`** (default **4 MiB**). A `Content-Length` over the cap is
  refused `413` before a byte is read; the read then counts anyway, because a
  chunked body declares no length and a header is not a constraint. Without it
  the SDK transport buffers whatever arrives — before a session, a message, or
  any other check exists.
- **`maxBatchSize`** (default **8**). A JSON-RPC batch longer than this is
  refused `-32600` before ANY of its messages is dispatched. Refusing partway
  through would have run exactly the tools the limit exists to bound.
- **`maxConcurrentCallsPerSession`** (default **4**). A batch is dispatched
  message-by-message with no waiting in between, so its calls all reach the
  host's tools at once; past this many, the rest queue. The catalogue staging
  they would each repeat is computed once and shared between them — `execute`
  still re-stages per call, so `canExecute` guards are evaluated at call time on
  state that may have moved.

## Tool projection

`tools/list` comes from `@agentkit/host`'s `ToolCatalog`, and each entry's
`AiToolDefinition` crosses **verbatim**: `name`, `description`, `inputSchema`.
In particular the tool's name is its own — *not* `<namespace>__<name>`. The
contributor namespace is AgentKit's attribution for a tool, not part of the
identifier, and rewriting it here would mean an MCP client and a chat turn call
the same tool by two different names.

`outputSchema` is deliberately not forwarded even when a definition has one: an
MCP tool that declares one promises `structuredContent`, and what this server
returns is the AgentKit envelope rendered as text.

`tools/call` returns the `AiToolEnvelope` the model would see in a chat turn:
the `summary` as its own leading text block when present, then the envelope's
payload (`modelData` when the tool set one, else its `data`) as one JSON text
block. `ok: false` sets `isError`, and a failed envelope's payload is the
structured `AiToolErrorData` (`errorCode` / `errorMessage` / `phase` /
`retryable`), so a caller reads a code rather than parsing a sentence. A partial
apply is flagged `isError` too — MCP has no third state, and reporting a
half-finished write as a clean success is the failure worth avoiding.

An unknown tool is a **JSON-RPC error**, not an `isError` result: the call was
never dispatched, and saying otherwise would tell the model its arguments
reached something.

## Writes

`writesEnabled` defaults to **false**, and with it off, tools declaring
`effect: "write"` are filtered on **both** paths — absent from `tools/list` and
refused by `tools/call` with the same "unknown tool" answer a nonexistent name
gets. Hiding alone would only stop a client that had not looked before.

The reason for the default: a write reached over MCP runs
`createProposalBuilderTool`'s pipeline exactly as it would in a turn — it stages
a proposal, and applies it if the `WritePolicy` allows — but no chat UI is
watching and nothing prompts a human. Turning writes on is a decision per
server, and it belongs in the host's wiring where someone can see it.

## `createStagedToolSource`

The default `McpToolSource`. It is in **this** package rather than in
`@agentkit/host` on purpose: `ToolCatalog` is deliberately definitions-only,
because handing out `AiTool.execute` would put a second, unguarded call path
next to the run loop's — and this function is exactly that second path. Keeping
it in the optional adapter means a host that never mounts an MCP server never
links it.

What keeps the path guarded rather than a bypass is that every call goes back
through `stageRegistry`, the same function the turn runner uses: the guard chain
runs (`isVisible` at staging, `canExecute` around `execute`), namespaces are
validated, unbound pruning applies, and the Ajv validator the registry compiled
from the tool's own `inputSchema` checks the arguments. None of it is
re-implemented here, which is the only way it cannot drift.

Cost: `contribute` runs once per `tools/list` and once per `tools/call`, because
which tools exist depends on the chat's bindings and on state a guard reads, and
both move between calls. A contributor whose `contribute` is expensive should
cache inside itself. The one thing that IS shared is the catalogue listing of a
session's concurrently-dispatched requests (see "Request bounds") — never the
staging `execute` does.

A host with its own execution path can implement `McpToolSource` directly — it
is two members, `{ catalog, execute }`.

## Not included

- **stdio transport.** This package is HTTP-only. A stdio MCP server is a
  process a client spawns, which is a different lifecycle from a host that is
  already running and already has state.
- **Resources, prompts, sampling, elicitation.** Tools only. The other MCP
  capabilities have no AgentKit port behind them yet.
- **A server.** `fetch` and `dispose`, nothing else: ports, TLS and lifecycle
  are the host's, and a transport package that started owning them would stop
  being optional.

## Development

```sh
bun run test:mcp-server      # from the repo root
bun run typecheck:mcp-server
bun run build:mcp-server
```
