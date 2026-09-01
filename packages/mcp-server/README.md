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
  // Which chat an MCP session works in, from ITS OWN headers, once, at init.
  sessionScope: (headers) => {
    const chatId = headers.get("x-agentkit-chat");
    return chatId === null ? {} : { chatId };
  },
  writesEnabled: false, // default; see "Writes" below
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

**Bind loopback.** Even with a token, publishing this to a LAN publishes tool
execution to a LAN.

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
cache inside itself.

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
