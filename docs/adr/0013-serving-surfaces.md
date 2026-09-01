# ADR 0013 — Serving surfaces: `@agentkit/mcp-server`, `@agentkit/client`, `@agentkit/react`

**Status:** accepted, implemented (2026-09-01–2026-09-02)
**Contract impact:** NONE at the DTO level. These are three new optional
adapter packages consuming the existing REST v1 and MCP surfaces, not new
schema. Revises [`docs/non-goals.md`](../non-goals.md)'s "React / UI
packages" entry.

## Problem

Two gaps blocked adoption, on two different sides of the wire.

**Server side**: [`docs/roadmap.md`](../roadmap.md)'s P7 (closed by [ADR
0011](0011-tool-governance.md)) named the MCP server package as the thing it
had to precede. A host had no way to expose its **own** tools as an MCP
server, which is how OpenPCB's assistant lets an external MCP client (an
IDE, another agent) drive it.

**Client side**: every existing chat surface — OpenPCB's frontend and
OneMind's `src-ts` frontend alike — was hand-coupled to its own bespoke
stream envelope (OpenPCB's `task.*` `EventSource` events, OneMind's
fetch-stream `{event}` JSON). Both would need a rewrite against
`@agentkit/transport-http`'s actual REST v1 + SSE surface regardless of
which one migrated first, which means the correct amount of client code to
write, centrally, was exactly **one** implementation — not two per-consumer
ports of the same resume/reconcile/idempotency logic
`@agentkit/transport-http` already got right on the server side ([ADR
0005](0005-http-transport.md)).

## Evidence

- The originating plan's consumer inventory, blocks-adoption item #8: "Both
  frontends are coupled to their own stream envelopes… need a shared
  client."
- OpenPCB's `assistant/backend/mcp/` as the `mcp-server` reference:
  constant-time bearer auth, a DNS-rebinding origin guard, session-per-client
  keyed on a stable header — never the client-announced name — tool
  projection reusing `AiToolDefinition` verbatim, write-tool filtering when
  writes are disabled. The same reference [`docs/roadmap.md`](../roadmap.md)'s
  prior Later entry for this package had already named.
- A Phase B/C fresh-context verifier pass, run against the shipped
  `mcp-server`, found that a leaked `Mcp-Session-Id` alone was sufficient to
  reach another caller's session: nothing checked that the caller presenting
  a session id was the same principal who had opened it.

## Decision

1. **`@agentkit/mcp-server`** — the official SDK's streamable-HTTP server as
   a fetch-native handler, over the **same** governed staging path [ADR
   0011](0011-tool-governance.md) built (`createStagedToolSource` reuses
   `stageRegistry`, so an MCP client sees exactly the tools a chat turn
   would). Constant-time bearer auth (web-crypto, no length oracle); a
   Host/Origin allowlist against DNS-rebinding; sessions keyed on the
   **server-minted** `Mcp-Session-Id`, never the client-announced
   `clientInfo.name` (a name a client can restate per call is a scope a
   client can borrow); write tools (`effect: "write"`) hidden from
   `tools/list` **and** refused by `tools/call` unless `writesEnabled` —
   hiding alone only stops a client that had not looked before, so both
   paths are filtered. `modelData`/`summary` project onto MCP results the
   same slim/full split the rest of the contract already makes (see
   [`docs/contracts.md`](../contracts.md)'s tool envelope).
2. **Hardened the next day: sessions are bound to the principal that opened
   them.** A fingerprint of the `Authorization` header is taken at the
   moment a session is opened and re-checked, in constant time, on every
   later request (GET, POST and DELETE alike) — a leaked session id
   presented by a *different* (even validly authenticated) principal gets
   the same 404 an invented id gets, so confirming "this session is not
   yours" never leaks "but it does exist." Sessions are also bounded:
   `maxSessions` (default 64, evicting the **oldest idle** session to make
   room — never refusing the newcomer, since nothing in the protocol obliges
   a client to ever send the DELETE that would free one) and an idle TTL,
   both reaped lazily on the request path rather than by an armed timer, so
   mounting the handler never keeps an event loop alive by itself.
3. **`@agentkit/client`** — one typed method per `REST_ROUTES` operation
   (compile-time exhaustive against the route table, the same
   "cannot-drift-by-transcription" property `transport-http`'s router
   already has, [ADR 0005](0005-http-transport.md)); `streamRun` as an
   auto-resuming async iterable on `Last-Event-ID`; `problem+json` responses
   surfaced as a typed `AgentKitClientError`; auto-minted `Idempotency-Key`s
   handed back to the caller so a timed-out submit can retry with the same
   key. Zero dependency beyond `@agentkit/contracts` and no `node:` import
   anywhere — the same client serves a web dashboard, an Electron renderer,
   and a CLI without a second implementation of resume semantics.
4. **`runPhase()`** realizes, as code, the status-vocabulary boundary [ADR
   0010](0010-chat-lifecycle-search-import.md) decided: a pure function of
   `{ status, events }` that derives `queued | running | streaming |
   waiting_approval | completed | failed | cancelled` from the host's
   canonical `RunStatusDto` plus the presence of streaming/terminal events on
   the log — never a fact the server itself publishes, so two tabs watching
   the same run cannot disagree about it, and no server implementer ever has
   to compute a "typing" bit slightly differently from another one.
5. **`@agentkit/react`** — headless hooks **only** (`useChat`, `useRun`,
   `useBranches`, `useProposals`, `useProviders`, one provider component).
   Deliberately no message list, no bubble, no composer, no spinner, no
   class name, no CSS import: what OpenPCB's and OneMind's chat UIs have in
   common is the **protocol** (submit, stream, reconcile, branch, approve) —
   what they do **not** share is the interface, and a component library at
   this layer would be a design system both consuming apps would have to
   fight rather than a dependency either could simply use. `react` is an
   **optional** peer (`>=18`) of the umbrella package, and nothing in the
   package imports `react-dom`, so the hooks work in a DOM renderer, React
   Native, or a custom reconciler alike.
6. **`useChat`'s optimistic pair applies streamed deltas by the same rule
   the host's own `RunProjector`** ([the custom-turn-executor seam,
   `packages/host/src/turn/projection.ts`](../architecture.md#custom-turn-executors))
   uses to reflect them into conversation state, then reconciles against a
   real `listMessages` at the terminal event — the client-side view of a
   running turn and the host's own persisted projection are computed by the
   same rule, so they cannot drift apart into two different in-flight
   renderings of one run.
7. **Revises [`docs/non-goals.md`](../non-goals.md)'s "React / UI packages"
   entry.** A first-party React package now exists, headless-only; the entry
   is narrowed to what remains true: no *styled* component library, no
   design system, ships from this repository.

## Alternatives considered

- **Ship components (a message bubble, a composer) alongside the hooks.**
  Rejected: the two consuming apps' interfaces do not share enough visual
  vocabulary to be served by one component library without one of them
  fighting it — a design system at this layer would cost more in awkward
  overrides than it would save.
- **Publish the client and `mcp-server` first, ahead of the lifecycle and
  governance work** ([ADR 0010](0010-chat-lifecycle-search-import.md), [ADR
  0011](0011-tool-governance.md)). Rejected implicitly by the actual landing
  order: a client typed against `REST_ROUTES` and an MCP server built on
  `ToolCatalog` are each only as complete as the surface underneath them —
  building them first would have meant re-typing the client twice (once per
  contract wave) instead of once against the finished `0.4.0` surface.
- **Key MCP sessions on the client-announced `clientInfo.name`** (simpler,
  no server-side id minting). Rejected as a repeat of exactly the defect
  [`docs/roadmap.md`](../roadmap.md)'s original Later entry for this package
  already named as a requirement to avoid: a name a client can restate per
  call is a scope a client can borrow, not a genuine identity.
- **Defer the session-fingerprint hardening**, reasoning that a
  server-minted id was already unguessable. Rejected once the verifier
  demonstrated the actual exploit: an unguessable id defeats *guessing* but
  not an id *leaked* (logged, proxied, shared) — the fix had to bind the
  session to the credential that opened it, not merely to an unguessable
  identifier.

## Consequences

- [`docs/roadmap.md`](../roadmap.md)'s old "MCP server package" and "Client
  SDK + React packages" Later entries both move to Done, referencing this
  ADR; their original reference-implementation notes (OpenPCB
  `assistant/backend/mcp/`) are preserved here rather than dropped.
- [`docs/non-goals.md`](../non-goals.md) no longer lists a client SDK or
  React hooks as deferred; a *styled* component library remains listed,
  narrower than before.
- Every session an `@agentkit/mcp-server` host serves now costs one extra
  constant-time comparison per request — a fixed, small, non-optional cost
  of the fix.
- `@agentkit/client` and `@agentkit/react` becoming real means
  [`docs/architecture.md`](../architecture.md)'s adapters-beside-host diagram
  gains two more entries that depend on `@agentkit/contracts`/`@agentkit/client`
  alone — neither is a dependency of `@agentkit/host` or `@agentkit/core`,
  preserving the "adapters depend on host, host depends on nothing above it"
  shape [ADR 0004](0004-mcp-client.md)/[ADR 0005](0005-http-transport.md)
  already established.

## Out of scope (deliberate)

A styled component library or design system; a non-React binding (Vue,
Svelte) over `@agentkit/client`; WebSocket transport (still open, [ADR
0005](0005-http-transport.md)); the mcp-server's unknown-vs-wrong-principal
404 responses being indistinguishable by **timing** as well as by body (both
return the identical 404 body, but the wrong-principal path does strictly
more work before answering it — a residual, undocumented-until-now side
channel, tracked in [`docs/roadmap.md`](../roadmap.md)'s Later list rather
than fixed here).
