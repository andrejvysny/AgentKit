# AgentKit native MCP adapters

This crate uses the official [`rmcp` Rust SDK](https://github.com/modelcontextprotocol/rust-sdk/tree/0cde3c5cf3e6aff0cc852ce6045f107e95991f48/crates/rmcp), pinned to `3.5.0`.
It offers native outbound stdio and Streamable HTTP clients, an outbound
`agentkit_core::Tool` bridge, and an authenticated inbound Streamable HTTP
server with host-provided tools, resources, and prompts.

## Protocol boundary

The supported revision list is frozen from the installed TypeScript SDK
`@modelcontextprotocol/sdk` **1.30.0**, `dist/esm/types.js`:

- `2025-11-25` (the outbound client's advertised revision)
- `2025-06-18`
- `2025-03-26`
- `2024-11-05`
- `2024-10-07`

The newer default `2026-07-28` in rmcp is deliberately excluded. Incoming
initialization with an unlisted revision is refused before an SDK session is
registered. `discover` is not exposed as a replacement lifecycle. The adapter
does not opt into MRTR, task extensions, OAuth flows, sampling, or elicitation.

## Outbound clients and native tools

`McpClient::stdio` launches an explicitly selected executable with arguments and
an explicit environment. Unlike the TypeScript SDK's curated inherited process
environment, native stdio starts with **no inherited environment**. Supply `PATH`
when the command or its children need path lookup, and required platform values
such as `SYSTEMROOT` on Windows. Secret values stay in the caller-provided
environment in memory; command/environment data is never copied into errors.
Child stderr is suppressed to prevent a server from leaking resolved credentials.
Call `close().await` to wait for SDK cleanup; drop also triggers SDK cancellation.

`McpClient::http` accepts a selected HTTPS endpoint or loopback HTTP endpoint and
an optional bearer token. `http_with_headers` accepts resolved custom headers.
Credentials in URL userinfo, query strings and fragments are refused. Redirects
and implicit environment proxies are disabled. Host/session/protocol/content
headers remain SDK-owned, and conflicting authorization sources are refused.
These explicit destination and environment rules are intentional native
constraints; they are not claims of identical TypeScript configuration behavior.

Defaults are 5 seconds for connection and request deadlines, 100 catalogue pages,
10,000 tools, a 1 MiB SSE event limit, and 4 MiB JSON-response/stdio-line limits. Repeated cursors and size/page overflow
fail closed. Calls are never automatically retried after timeout, cancellation,
disconnect or an expired session. The SDK's transparent expired-session replay
is disabled because delivery can be ambiguous for a write. A caller may
explicitly reconnect for a later operation. `McpClientManager` adds lifecycle,
secret resolution, and bounded connection resilience as described below.

`McpToolSet::new(alias, tool_aliases, client)` produces a bounded contribution
using `contribute(cancellation)`. Returned tools implement `agentkit_core::Tool`
and can be registered with `ToolRegistry`. Their stable capability is
`mcp.<serverAlias>.<effectiveToolName>`; their provider-safe name replaces each
`.` with `__`. The canonical and provider-safe identity sets are checked before
any contribution is returned. `contribute_all` also rejects cross-server name
collisions before returning a partial catalogue. Unlike the TypeScript manager's
degraded contribution, a failed server in `contribute_all` fails this explicit
operation; a host that wants per-server degradation can call `contribute` per
server and record the failure itself.

## Managed outbound sessions

`McpClientManager::new(configs, secrets)` accepts `ServerConfig` declarations and
an injected `McpSecretResolver`. The serde config has the TypeScript fields
`alias`, `transport`, `secretRefs`, `enabled`, `toolAliases`, and `resilience`;
durable host DTO records can be deserialized into this runtime view without
copying resolved credentials into storage. Secret substitution applies only to
stdio environment values and HTTP header values. Commands, arguments, and URLs
are left unchanged. Errors contain fixed messages and machine-readable `code()`
values, never resolved material or SDK causes.

The default connector uses the official transports. Managed stdio inherits the
exact curated SDK 1.30.0 environment allowlist beneath declared overrides, skips
values starting with `()`, and suppresses stderr. Direct `McpClient::stdio`
retains its explicit-environment API. Managed HTTP retains the destination,
redirect, proxy, and reserved-header restrictions above.

Defaults match TypeScript: 5-second request/connect deadlines, three attempts
per connection cycle, jitter-free exponential connect delays of 250 then 500 ms
(factor 2, ceiling 2000 ms), a 5000 ms hard circuit lockout after a failed cycle,
and two reconnect rounds per eligible request. The first connection after
lockout runs a full new cycle. Invalid configuration, missing secrets,
cancellation, and unsupported revisions do not arm the circuit. Connection and
reconnection are serialized per server; concurrent failed reads converge on one
new connection generation. Explicit `connect` reopens a deliberately closed
session; requests never reopen a closed or revoked lifecycle.

Catalogue reads can reconnect after a disconnected transport. `retryTimeouts`
defaults to false and enables timeout replay for catalogue reads only. A tool
call reconnects only when `Closed` proves it was rejected before dispatch.
Dispatched calls never replay after disconnect, timeout, cancellation, or remote
failure, even with `retryTimeouts: true`. This intentionally strengthens the
TypeScript rule: its SDK `ConnectionClosed` path can replay a mid-call write,
and its timeout opt-in also applies to calls. A custom `McpConnection` must reserve
`Closed` for a provably unsent request and use `Disconnected` after dispatch.

Secrets are re-read on every connection attempt. A resolver that implements
`generation` enables rotation detection before the next operation: the manager
closes the old connection, clears its failed circuit, and resolves new values.
Generation is checked before and after connecting to reject material that
changed during setup. The default generation is zero, so a resolver without a
revision feed picks up rotation on reconnect. Immediate revocation uses
`revoke()`; `dispose().await` additionally waits for transport cleanup. Hosts must
advance generation when a referenced credential changes or becomes inaccessible.

`connect_all` reports connected, failed, and disabled aliases independently.
`contribute` preserves healthy tools while reporting an unavailable server;
identity collisions still refuse the affected catalogue. `call_tool` accepts
canonical identities and resolves configured tool aliases back to remote names.
`replace_configs` validates the new batch before replacing it, preserves
unchanged sessions, and revokes changed/removed sessions and retained old tool
handles before publishing replacements. `dispose` permanently revokes every
session. A manager must be scoped to the embedding host's authority boundary;
it does not invent user or actor permissions. Inbound actor grant revocation
continues through `Backend::revoke_actor`.

`with_ports` injects the connector and clock for deterministic fault tests.
Connector futures must release partially opened transports when dropped and
must observe their cancellation token before returning late resources. Native
timing options use nonnegative integer milliseconds, positive attempt/deadline
values, and a finite reconnect factor at least one; invalid native options fail
configuration validation.

Descriptions and input schemas pass through unchanged. Only
`annotations.readOnlyHint: true` makes an outbound tool read-only; all others
are writes. The complete remote content stays in `ToolResult.data`. Model data
contains capped UTF-8 text and object-valued structured content; non-text parts
produce a warning. A remote `isError` result becomes an unsuccessful tool result.
Transport errors become unsuccessful envelopes with `retryable: false`. Summary
truncation respects Unicode scalar boundaries; exact JavaScript lone-surrogate
summary slicing is not reproduced.

Inbound JSON uses JavaScript binary64 parsing before SDK dispatch. Unknown
argument values that overflow remain raw numeric nodes, readable as infinity
through `agentkit_contracts::javascript_number`; they are not changed to null
before the host callback. Tool validation uses the shared Ajv-compatible draft-07
compiler (`strict: false`, all errors, and formats), including its numeric
keywords. Response values and outgoing call arguments apply JavaScript
`JSON.stringify` rules: nonfinite numbers become null and finite numbers use
JavaScript rounding. This includes structured content and backend error data.

With serde_json's arbitrary-precision feature, its ordinary `Value` visitor can
mistake a user object containing `$serde_json::private::Number` or
`$serde_json::private::RawValue` for an internal token. The native SDK boundary
recursively escapes those keys and its own escape
prefix before SDK parsing, then restores keys directly in callback arguments and
returned schema/content/metadata fields. This is a bijection over keys; string
values are never replaced. The HTTP adapter applies it to JSON and SSE responses;
stdio uses the official SDK async reader/writer transport over a bounded protected
line reader. Server results normalize their JSON fields directly rather than
deserializing a serialized typed result again. The escape representation never
appears on the public MCP wire or in host callback values.

The core tool bridge carries `ToolContext.dispatch_guard` through connection and
reconnection. The last check occurs after those awaits and immediately before
SDK dispatch; a revoked/expired lease returns `mcp_authority_revoked` without a
tool call. This preflight cannot atomically fence a remote mutation. External
write implementations must enforce trusted execution identity in the same
transaction as the effect. Where that is unavailable, a successful local check
does not prove the lease remained current while a remote effect ran, and the
single-send/ambiguous-delivery rule still applies.

## Inbound host integration

Construct `McpServer` with an `Arc<dyn Backend>`, an `Arc<dyn Authenticator>`, and
`ServerOptions`. `router()` returns an Axum router rooted at `/`; mount it at the
host's chosen MCP path, or use `handle` in an existing server. The adapter opens
no listening socket. `BearerAuth` is a constant-time hashed-token implementation;
custom authenticators can select a trusted principal and optional chat from
headers. Session ownership is bound to the authorization-header fingerprint,
so custom authentication must supply an authorization header too.

Authentication and Host/Origin checks run before parsing bodies or looking up
sessions. Sessions get server-generated actor IDs and retain the authenticated
principal/chat for their lifetime. Another valid credential cannot borrow a
session ID. Write tools are hidden from listing and refused on execution unless
`writes_enabled` is explicitly enabled. Every execution re-reads the host's
catalogue and validates arguments against its current input schema. Resources
and prompts are enabled only through `Backend::features`; guessed hidden names
are refused. Prompt arguments reject missing required or unknown names and
non-string values.

After catalogue awaits and semaphore acquisition, the adapter rechecks request
cancellation, operation cancellation, and actor-lease revocation immediately
before backend dispatch. A catalogue that cancels its scope and returns in the
same poll cannot dispatch a subsequent tool effect.

`Backend` must apply the same staging, guards, proposal pipeline, and durable
effect logging as the embedding host. This adapter does not itself grant write
authority or persist receipts. `SessionScope.cancellation` is per-operation and
inherits session cancellation; request cancellation, deadline and shutdown cancel
it. Cancelling a future does not undo external effects. The host must preserve
evidence for effects that already occurred and must not repeat ambiguous writes.
`revoke_actor` must synchronously revoke any actor-bound grants. The adapter calls
it once on session deletion, idle/capacity eviction, shutdown or final drop.

Defaults: writes disabled; loopback Host allowlist; no Origin check unless
configured; 64 sessions per credential, global cap 16 times that; 30 minute idle
TTL; 4 MiB request body; batch ceiling 8; 4 concurrent operations per session;
120 second operation deadline; 200 tools per result page. Idle sessions are
reaped lazily on authenticated requests. Capacity eviction removes the oldest
idle session before refusing a new session; active operations are protected.
Call `close().await` to revoke grants and await session cleanup at shutdown.

The current implementation has not established full TypeScript MCP parity.
Not covered by this crate's acceptance evidence: bounded SDK notification/event
replay after broken HTTP streams, batch request behavior across every revision,
exact HTTP error-body parity, and
operating-system-specific process cleanup under abrupt host termination. These
remain explicit qualification gates, not inferred successes.

## Validation

```sh
# Run bun install --frozen-lockfile at the repository root first.
cargo test -p agentkit-mcp --locked
cargo clippy -p agentkit-mcp --all-targets --locked -- -D warnings
```

Integration tests use native HTTP sockets plus the installed official TypeScript
SDK over a real stdio child process. They cover all five negotiated revisions,
rejection of the newer SDK default, tool pagination/cursor bounds, real registry
integration, aliases/collisions, argument checks, full versus model projection,
UTF-8 bounds, hidden writes/resources/prompts, session ownership, actor grant
revocation, body/Host/Origin limits, deadline cancellation, and a child that
records a write then exits before returning a result. The latter verifies one
side effect and no automatic retry. These are local deterministic tests, not
live third-party server qualification or cross-platform release evidence.

Managed-session tests additionally cover exact connection/reconnect budgets and
delays, hard circuit expiry, shared reconnect, degraded contribution, alias
routing, missing secrets, rotation before dispatch, config replacement and
retained-handle revocation, close/connect races, connect/request deadlines and
cancellation, and ambiguous calls with one effect. Native HTTP rotation verifies
the previous remote session's actor is revoked before the new session executes;
a real unresponsive stdio process verifies connect-timeout process cleanup.
