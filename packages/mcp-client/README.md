# @agentkit/mcp-client

Bridges [Model Context Protocol](https://modelcontextprotocol.io) servers into
AgentKit runs: an `McpClientManager` owns the connections, and a
`ToolSetContributor` built from it turns every connected server's tools into
`AiTool`s that a `TurnRunner` stages like any other tool source.

Built on the official `@modelcontextprotocol/sdk` (`Client`,
`StdioClientTransport`, `StreamableHTTPClientTransport`). This package adds only
what an agent host needs on top of it: stable tool identity, per-server failure
isolation, and secrets that never reach a log line.

## Wiring

```ts
import { McpClientManager, createMcpToolSetContributor } from "@agentkit/mcp-client";

const mcp = new McpClientManager(
  { secrets, logger, clock },
  [
    {
      alias: "github",
      transport: {
        kind: "stdio",
        command: "gh-mcp-server",
        env: { GITHUB_TOKEN: "${GH_TOKEN}" },
      },
      secretRefs: { GH_TOKEN: "provider.github.token" },
      toolAliases: { list_issues: "issues" },
    },
    {
      alias: "docs",
      transport: {
        kind: "http",
        url: "https://docs.internal/mcp",
        headers: { Authorization: "Bearer ${DOCS_TOKEN}" },
      },
      secretRefs: { DOCS_TOKEN: "docs.mcp.token" },
      resilience: { requestTimeoutMs: 15_000 },
    },
  ],
);

const runner = new TurnRunner({
  /* ... */
  contributors: [hostTools, createMcpToolSetContributor(mcp)],
});

// On shutdown — closing a stdio transport reaps the child process.
await mcp.dispose();
```

`contribute()` connects (or reconnects) every enabled server at the start of a
run, so a server that was down last turn comes back this turn without extra
wiring. A server that stays down costs its own tools and nothing else.

The contributor's `namespace` is `mcp`, one of AgentKit's **reserved**
namespaces — which is why it also sets the framework-internal
`privileged: true`. That flag is not an extension point: a host bridging tools
of its own picks a namespace of its own (`^[a-z][a-z0-9_-]*$`, and not
`agentkit`/`chat`/`mcp`). See [`docs/ports.md`](../../docs/ports.md#toolsetcontributor).

## Tool identity

Each tool gets a canonical id `mcp.<serverAlias>.<effectiveToolName>`, where
`effectiveToolName` is `toolAliases[name] ?? name`. Aliases must match
`^[a-z][a-z0-9-]*$` and tool names `^[a-z][a-z0-9_-]*(?:\.[a-z0-9_-]+)*$`;
anything else is a typed error rather than a mangled name.

The canonical id is the **routing key** and lands on
`AiToolDefinition.capability`. `AiToolDefinition.name` carries a projection of
it with dots replaced by `__` (`mcp__github__issues`), because `AiToolRegistry`
and provider function schemas reject dots — registering the dotted form would
get the tool silently dropped during registry staging.

Collisions **fail closed** at both levels: within one server's tool batch, and
across servers when the contribution is assembled. Neither is resolved by
last-write-wins, because the model would then call a tool it was shown and reach
a different implementation.

`inputSchema` is passed through **verbatim** — nothing widened, nothing
tightened, no `additionalProperties` injected. `annotations.readOnlyHint === true`
maps to `effect: "read"`; everything else maps to `"write"`, conservatively.

## Secrets

`secretRefs` maps a `${placeholder}` token to a `SecretStore` ref. At connect
time the resolved value replaces the token inside **stdio `env` values** and
**http `headers` values** only — never in a command, an argv entry or a URL,
where it would end up in `ps` output or a proxy log.

A ref the store answers `null` for is an `mcp_secret_missing` error naming the
placeholder and the ref, never a value. Every message built from config-derived
text is redacted before it is thrown or logged: resolved values are replaced
with `***`.

## Persisting the server list — `McpServerConfigStore`

`McpServerConfig` is a value a host can declare in a file and be done with. A
host that lets a **user** add servers at runtime needs somewhere to put them,
and `McpServerConfigStore` (`src/config-store.ts`) is that port:
`create`/`update`/`delete`/`get`/`list` over `McpServerConfigRecord`
(`McpServerConfig` plus `id`, `createdAt`, `updatedAt`).

It is deliberately **not** part of `AssistantStore`. That aggregate exists so
the writes that must land together can; an MCP server config shares a
transaction with nothing, and a seventh member would make every adapter — and
every hand-rolled store — implement a port most hosts never use. The reference
implementations are standalone classes beside the assistant store:
`MemoryMcpServerConfigStore` (`@agentkit/adapters-memory`) and
`SqliteMcpServerConfigStore` (`@agentkit/adapters-sqlite`, over the same
database handle or path, `mcp_servers` in `SCHEMA_V8`). Both are graded by
`describeMcpServerConfigStoreConformance` from `@agentkit/testing`.

**`alias` is unique, case-sensitively.** It is the tool namespace every
canonical id embeds, so two servers sharing one would mint the same id for two
different tools — which `resolveMcpToolIdentity` refuses at staging time, as a
hard failure of the whole run, long after the record that caused it was
written. Refusing the write (`mcp_invalid_config`) is the same rule applied
where it can still be acted on. `id` is separate from `alias` precisely so a
rename does not break the handle a URL or a foreign key uses. An unknown id on
`update`/`delete` is `mcp_config_not_found`.

No secret material is stored: a record carries `secretRefs`, and the values
behind those refs are resolved at connect time and written nowhere — which is
what lets `@agentkit/transport-http` publish the whole map over
`/v1/mcp/servers`.

## Resilience defaults

All overridable per server via `resilience`.

| Option | Default | What it governs |
| --- | --- | --- |
| `requestTimeoutMs` | `5000` | Deadline for one `tools/list` / `tools/call`. |
| `connectTimeoutMs` | `5000` | Deadline for one connect attempt (transport + `initialize`). |
| `maxConnectAttempts` | `3` | Attempts in one connect cycle before the circuit opens. |
| `connectBackoffBaseMs` | `250` | First backoff step; doubles per attempt. |
| `connectBackoffMaxMs` | `2000` | Backoff ceiling. No jitter. |
| `circuitOpenMs` | `5000` | Lockout window after a failed cycle. |
| `reconnectMaxAttempts` | `2` | Reconnect+retry rounds for one auto-retryable request failure. |
| `reconnectBackoffFactor` | `2` | Growth factor for the reconnect backoff. |
| `retryTimeouts` | `false` | Whether a request timeout may be replayed. See below. |

**Only a request that never reached the server is auto-retried.** The reconnect
loop fires on `mcp_not_connected` — the session was already dead — and on
nothing else. A request **timeout is not** retried: our deadline firing says
nothing about whether the server ran the tool, so replaying one would execute a
slow-but-successful write two or three times while the model is told the call
failed. Set `retryTimeouts: true` per server to opt back in, for a server whose
tools are all idempotent.

The circuit is a **hard timed lockout**, not a half-open probe: once a cycle
fails, `connect()` rejects immediately with `mcp_circuit_open` until the window
elapses, and the first call afterwards runs a fresh full cycle. A failure
retrying cannot fix (a missing secret, an unparseable URL) does *not* arm the
lockout — the precise error stays visible.

Concurrent request failures share **one** reconnect: two calls that die on the
same dropped session produce one reconnect and both retry after it.

**Both deadlines are races, not awaits.** Aborting a signal is a request, and a
transport that ignores it (a spawn that never execs, an SDK path that only looks
between round trips) would otherwise hold the deadline open forever — and,
because the connect promise is shared, take every later turn down with it while
the circuit breaker waits for a failure that never arrives. So the timeout wins
the race and the caller gets `mcp_connect_failed` / `mcp_request_timeout` on
time. The abandoned attempt still cleans up after itself: a connect that
completes after its deadline (or after a `close()`) closes the client and
transport it built instead of installing them.

**`close()` waits for a connect — and for a RECONNECT — in flight.**
`McpClientManager.dispose()` therefore returns only once every server's connect
has settled and been torn down — the alternative is a `Client`, and over stdio a
child process, adopted a moment after the manager reported everything closed. A
reconnect is a teardown followed by an open, so a close landing between its two
halves is checked on both sides of them.

**A closed session stays closed.** Only an explicit `connect()` re-opens one:
the request path (and the reconnect it drives) refuses a disposed session with
`mcp_not_connected` rather than reviving it behind `dispose()`'s back.

## Errors

Every failure is an `McpError` with a stable `code` and a `retryable` verdict,
classified where the cause is known (our own timer, the transport's `onclose`,
the SDK's JSON-RPC error code) — never by matching message text. `retryable` is
**advisory**: it says a retry could succeed, not that one is safe. The host and
the model decide, because they know what the tool does; the client's own
auto-retry gate is the narrower set above.

`mcp_circuit_open`, `mcp_connect_failed`, `mcp_request_timeout`,
`mcp_request_aborted`, `mcp_reconnect_exhausted`, `mcp_not_connected`,
`mcp_remote_error`, `mcp_secret_missing`, `mcp_canonical_id_collision`,
`mcp_invalid_alias`, `mcp_invalid_tool_name`, `mcp_invalid_config`,
`mcp_invalid_canonical_id`.

A tool call never throws into the run loop. Failures come back as an
`ok: false` result whose `modelData` is `{ errorCode, errorMessage, retryable }`
— the same shape core's own error envelope uses, so the code survives into what
the model reads and it can treat `mcp_request_timeout` differently from
`mcp_remote_error`. A server-reported failure (`isError: true`) uses the code
`mcp_tool_error`.

## Result size

The model-facing half of every result — the success text, a server-reported
error's explanation, a bridge failure's message — is capped at the run's
`limits.maxBytes` and the result is flagged `truncated`. The text arrives from a
server that was never told the run's budget, and an uncapped one is replayed
into context on every later turn of the chat. The `data` payload keeps
everything the server sent, for a UI to render.

## Testing

`McpClientManagerDeps.transportFactory` is injectable, so tests drive real MCP
servers over the SDK's `InMemoryTransport` instead of spawning processes or
opening sockets. See `tests/helpers.ts`.
