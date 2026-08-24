# ADR 0004 — `@agentkit/mcp-client`: MCP servers as a tool source

**Status:** accepted, implemented (2026-08-24)
**Contract impact:** NONE. `CONTRACT_VERSION` stays `0.2.0` — this package is
an optional adapter beside `@agentkit/host` (`ToolSetContributor` +
`AiTool`), not a change to `@agentkit/contracts`.

## Problem

`docs/roadmap.md`'s P2 named the goal: bridge Model Context Protocol servers'
tools into AgentKit runs as a `ToolSetContributor`, preserving the semantics
OneMind's MCP integration already proved in production. OneMind's own
implementation, however, is not something to port as-is — it runs **no
official SDK**, a hand-rolled JSON-RPC client instead, and that client
carries defects worth fixing rather than copying: its stdio transport frames
messages with LSP-style `Content-Length` headers, which is incompatible with
MCP's stdio spec (newline-delimited JSON-RPC) and only happens to work
against OneMind's own future servers by accident; it hardcodes two
independent protocol version strings that can drift from each other and from
the spec; it decides retryability by matching substrings in error messages,
which silently rots the first time a dependency rewords a string; its MCP
server credentials are stored in plaintext and returned to callers over REST;
it forces `additionalProperties: true` onto every tool schema it advertises,
which is not what the server actually declared; and it has no disposal path
on shutdown, so spawned stdio server processes leak. None of that is safe to
copy verbatim — but the canonical-id grammar, the collision handling, and the
resilience numbers it settled on in production are exactly the kind of
battle-tested detail worth keeping.

## Evidence

- OneMind's MCP layer (`src-ts/src/infrastructure/mcp/` +
  `domain/services/mcp/`, per `docs/roadmap.md` P2) is the reference for
  semantics — canonical tool ids, resilience defaults, orphan
  reconciliation, secrets-via-store — while its transport and protocol
  handling are explicitly not carried forward (see Problem above).
- AgentKit already has a balanced-history invariant `runChat()` depends on
  (every declared `tool_call_id` gets exactly one matching `role: "tool"`
  message) and a durable event log whose writes are not one transaction
  (`TurnRunner.projectEvent` writes the assistant message on
  `run.message.completed` and each tool result on the `run.tool.*` that
  follows it, as **separate** store calls) — so a crash between them is a
  pre-existing hazard this phase had to close for MCP-sourced tool calls the
  same way it must be closed for any other tool source.

## Decision

Build on the official `@modelcontextprotocol/sdk` `^1.30.0` (`Client`,
`StdioClientTransport`, `StreamableHTTPClientTransport`) for protocol
handling, and preserve OneMind's semantics on top of it:

1. **Canonical tool ids `mcp.<serverAlias>.<effectiveToolName>`**, OneMind's
   grammar carried over verbatim (`packages/mcp-client/src/identity.ts`).
   `effectiveToolName` is `toolAliases[name] ?? name`, a host-chosen rename
   applied before the id is built.
2. **Collisions fail closed at two levels**, not one: within one server's
   tool batch (`buildMcpToolIdentityIndex`) and again across servers when a
   run's contribution is assembled (`createMcpToolSetContributor`'s
   `byCanonicalId`/`byRegistryName` maps). Neither level resolves by
   last-write-wins — the model would then call a tool it was shown and reach
   a different server's implementation with the arguments it wrote for the
   other one.
3. **Resilience defaults matching OneMind's production numbers, overridable
   per server**: 5s request timeout, 5s connect timeout, 3 connect attempts,
   exponential backoff 250 → 2000ms, a 5s **hard timed lockout** circuit
   breaker with no half-open probe (an MCP connect is expensive — a process
   spawn, an `initialize` round trip — so letting one caller "test the
   water" mid-window just reintroduces the storm the lockout exists to
   stop). Connect resilience and request resilience answer different
   questions and are governed separately; both apply to **both** transports
   (`packages/mcp-client/src/resilience.ts`, `session.ts`).
4. **Reconnect dedup, extended to both transports.** `McpSession` tracks a
   `generation` counter bumped on every successful open and a shared
   `reconnecting` promise; a caller that fails and finds the session already
   past its generation has nothing to reconnect — someone else already did.
   Concurrent request failures on one dropped session therefore produce
   exactly one reconnect, not one per caller, regardless of whether the
   session is stdio or streamable-HTTP.
5. **Typed `McpError { code, retryable }`**, 13 codes
   (`packages/mcp-client/src/errors.ts`), classified where the cause is
   known — our own timer, the transport's `onclose`, the SDK's JSON-RPC
   error — never by matching message text, closing the exact defect named in
   Problem.
6. **Secrets resolve through `SecretStore`, redacted everywhere else.**
   `secretRefs` are resolved once at connect (`secrets.ts`) and substituted
   only into stdio `env` values and http `headers` values — never a command,
   an argv entry, or a URL, where they would surface in `ps` output or a
   proxy log. Every message built from config-derived text is redacted
   (`***`) before it is thrown or logged. OneMind's plaintext-in-SQLite,
   returned-over-REST pattern is not copied.
7. **Registry-name projection is a second name, not a rename.** MCP tool ids
   are dotted; `AiToolRegistry` and every provider's function-calling schema
   validate against `TOOL_NAME_PATTERN`, which bars dots. `toRegistryToolName`
   projects the canonical id onto a registry-legal name (`.` → `__`,
   checked, not assumed) and that projection becomes `AiToolDefinition.name`
   — the model-facing identity. The canonical id itself is kept on
   `AiToolDefinition.capability`, so it survives as the routing and
   collision key across restarts even though the registry never sees it.
8. **Tool `inputSchema` passes through verbatim.** Nothing widened, nothing
   tightened, no `additionalProperties` injected — the exact OneMind defect
   this phase declines to copy.
9. **Orphan tool-call reconciliation lives at the HOST layer, not in
   `mcp-client`.** `packages/host/src/turn/history-reconcile.ts`'s
   `reconcileOrphanToolCalls`, called from `TurnRunner.assembleMessages`,
   synthesizes an in-memory `tool_result_missing` failure for any assistant
   turn whose declared `tool_calls` never got an answer — the crash window
   is `projectEvent`'s two separate writes (see Evidence). This belongs at
   the host, not the MCP bridge, for two reasons: the orphan can come from
   *any* tool source, MCP or otherwise, and the synthetic result is never
   persisted — the records stay the truth about what happened, which is that
   no result was ever produced; writing a fake result into the store would
   make a crash permanently indistinguishable from a tool that ran.

## Alternatives considered

- **Copy OneMind's transport implementations directly.** Rejected: they are
  non-conformant with the MCP spec on the wire (LSP framing on stdio) and
  would only interoperate with OneMind's own future servers by coincidence,
  not with real MCP servers.
- **Put MCP SDK types in `@agentkit/core` or `@agentkit/host`.** Rejected:
  the SDK is a dependency this package needs, not one every consumer of
  core/host should carry whether or not it uses MCP — the types stay
  package-local, and `mcp-client` is an optional adapter beside host, the
  same shape `@agentkit/transport-http` takes (see [ADR
  0005](0005-http-transport.md)).
- **Persist the synthetic orphan tool result into the durable event
  log/store.** Rejected: the records are the truth about what happened, and
  reconciliation is in-memory precisely so a crash stays distinguishable
  from a tool that genuinely ran, permanently.

## Consequences

- `@agentkit/mcp-client` depends on `@agentkit/host` and `@agentkit/contracts`
  but nothing in `host` or `core` depends on it — a host that never
  configures an MCP server pays nothing for this package existing.
- Orphan reconciliation, though it shipped in the same commit as this
  package, is generic to any tool source and lives in `packages/host`; a
  future non-MCP tool bridge with the same crash-window hazard benefits from
  it automatically, with no bridge-specific wiring.
- Write-capable MCP tools (`effect: "write"`, derived from
  `annotations.readOnlyHint`) are exposed as ordinary run tools in this
  phase — they are **not** yet routed through `createProposalBuilderTool`.
  Wiring MCP writes into the proposal pipeline (the improvement over OneMind
  the roadmap originally flagged for this phase) did not ship; it remains
  open work.
- `docs/roadmap.md`'s P2 entry moves to Done referencing this ADR.

## Out of scope (deliberate)

Routing `effect: "write"` MCP tools through the proposal pipeline
(`createProposalBuilderTool`) — tracked as open work, not carried by this
ADR; an MCP server package (exposing an AgentKit host itself as an MCP
server — see `docs/roadmap.md`'s Later list); resource/prompt primitives
from the MCP spec beyond tools; per-user or per-tenant MCP server allowlists
(a host fronts `McpToolSource` with its own policy layer if it needs one —
the contributor's dependency is structural for exactly this reason).
