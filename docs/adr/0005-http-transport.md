# ADR 0005 — `@agentkit/transport-http`: the REST v1 adapter

**Status:** accepted, implemented (2026-08-24)
**Contract impact:** NONE. `CONTRACT_VERSION` stays `0.2.0` — this package
serves `packages/contracts/src/rest.ts` as written; it adds no DTOs and
changes no schema.

## Problem

`docs/roadmap.md`'s P3 named the goal: an official, optional adapter serving
the REST v1 surface `@agentkit/contracts` already declares as types and
schemas (`REST_ROUTES`, the request/response DTOs) but that no package in
this repository implements. Two reference behaviors needed to be gotten
right rather than assumed: how `streamRun` resumes a dropped SSE connection,
and how a client that never sees a 201 for a submit is kept from duplicating
a turn.

## Evidence

- **OpenPCB's tasks-module SSE endpoint** implements replay-then-subscribe:
  replay the durable log, then attach to a live feed. That design has an
  unguarded gap — events appended between the last row the replay read and
  the moment the subscription attaches land in neither — and it carries **no
  `Last-Event-ID` support at all**, so a dropped connection cannot resume
  short of a full replay regardless. Its own cloud `copilot-client.ts`, by
  contrast, proves the alternative pattern actually works in production: an
  id/seq-keyed resume that a reconnecting client drives itself.
- **OneMind**'s stream-service policy — auto-resume only on a crash, and
  keeping the stream open across a whole tool chain rather than per
  provider call — is the reference for what "the stream is still going"
  should mean to a client.

## Decision

1. **Fetch-standard, zero-framework handler.** `createRestHandler(deps)` is a
   plain `(Request) => Promise<Response>` built only on `Request`/`Response`,
   `ReadableStream`, `URL`, `crypto.subtle`, and `TextEncoder` — no HTTP
   framework, no router library, **no npm dependencies** at all. It runs
   under `Bun.serve`, under Node ≥ 19, or mounted inside a framework that can
   hand over a standard `Request` (e.g. Hono).
2. **The router is compiled from `REST_ROUTES`, not transcribed from it**
   (`packages/transport-http/src/router.ts`). Handling is dispatched by
   `RestOperation`, so a route added to the contract fails this package's
   compile until a handler exists for it — the contract and the server
   cannot drift apart by someone retyping a path.
3. **SSE `streamRun` is replay-then-poll on a `seq` cursor, deliberately not
   replay-then-subscribe** (`packages/transport-http/src/sse.ts`). "What have
   I sent?" is a number the reader owns, and every read is defined relative
   to it, so the gap OpenPCB's design has is structurally impossible here —
   the cost is one poll interval of latency, the benefit is that replay,
   resume, and live-follow collapse into one code path instead of three that
   have to agree. `Last-Event-ID` carries an `AiRunEvent.eventId`;
   `resolveStartSeq` finds its `seq` in the log and resumes one past it. An
   **unknown** id replays the whole log from the start rather than failing —
   a client holding an id from another run cannot be resumed from, and a
   full replay is the only answer that leaves it consistent (and one a
   client can dedupe by `eventId`, unlike a partial stream). The stream
   closes on a terminal run event, and also when the task itself is terminal
   but its log holds no terminal event (a crashed attempt, a task cancelled
   before its worker wrote anything) — without that second rule such a
   stream polls forever against a run that will never speak again.
4. **`Idempotency-Key` is required on `submitMessage`**, answering `400
   idempotency_key_required` without one — this is the one write that
   creates three records at once (a task, the user message, the assistant
   placeholder), and a retried POST without a key is indistinguishable from
   a user who really did send twice. The key becomes the run id
   deterministically: `taskId = "task_ik_" + sha256hex(chatId + ":" +
   idempotencyKey)` (`packages/transport-http/src/idempotency.ts`), so the
   same key resolves to the same run from any process, after any restart, in
   any replica. It rides `TurnRunner.submitMessage`'s existing per-`taskId`
   idempotency — a second submit under the same derived id writes nothing
   and returns the first submit's ids. First submit answers 201; a replay of
   the same `(chatId, key)` answers 200 with the identical body.
5. **Errors are RFC 7807 `application/problem+json` everywhere**, with a
   single host-`code` → HTTP-status table
   (`packages/transport-http/src/problem.ts`) rather than per-call-site
   status choices — two call sites mapping the same code to two different
   statuses is exactly the drift the table exists to prevent. `code` is what
   a client branches on; `type`/`title` are for humans.
6. **`501` is a deliberate policy, not a missing feature.** `GET /v1/tools`
   answers 501 unless `deps.toolCatalog` is supplied: `ToolSetContributor
   .contribute` is a per-*run* call taking a chat's bindings, limits, and
   scope, and this route names no chat, so synthesizing a run context would
   advertise a tool set no actual turn would receive — this needs a
   chat-independent tool-enumeration port that does not exist yet (tracked
   in `docs/roadmap.md`'s Later list). The proposal decision routes
   (`approve`/`reject`/`apply`) answer 501 the same way when `deps.proposals`
   is not wired. Both are optional dependencies precisely so a host can run
   the transport before it has every capability wired.

## Alternatives considered

- **`OutboxStore`-driven SSE (subscribe to the live feed after replay).**
  Rejected: the unguarded gap OpenPCB's implementation has (see Evidence).
  The `OutboxStore` port itself is not removed — it stays the mechanism for
  push-style publishers (a websocket, a message bus) that this transport
  does not attempt; `streamRun`'s cursor-poll design is specific to this
  request/response-shaped HTTP adapter.
- **Assume every client is a browser `EventSource`.** Rejected: `EventSource`
  cannot set request headers, and this adapter's `authenticate` hook and
  `Idempotency-Key`/`Last-Event-ID` handling assume a client that controls
  its own headers (a `fetch`-based SSE reader, or a non-browser client) —
  documented in `packages/transport-http/README.md` rather than silently
  assumed.

## Consequences

- `@agentkit/transport-http` depends on `@agentkit/host` and
  `@agentkit/contracts` but nothing in `core`/`host` depends on it — the same
  optional-adapter shape `@agentkit/mcp-client` takes (see [ADR
  0004](0004-mcp-client.md)); a host is always free to implement its own
  transport directly against the host ports instead.
- `GET /v1/tools` stays 501 for any host that has not built a
  chat-independent tool-enumeration port — this is now the concrete blocker
  recorded against that roadmap item, not an abstract future concern.
- `docs/roadmap.md`'s P3 entry moves to Done referencing this ADR; its
  original wording ("follow live via `OutboxStore`") is corrected there —
  the shipped design is cursor-poll, not outbox-subscribe.
- WebSocket transport remains unimplemented; SSE plus polling submit/read is
  the whole surface this phase ships.

## Out of scope (deliberate)

WebSocket transport; a chat-independent tool-enumeration port (blocks `GET
/v1/tools` leaving 501); a generated client SDK against this surface (see
`docs/roadmap.md`'s Later list — never a dependency of the headless
framework); per-principal authorization beyond the opaque `authenticate`
hook (the contract has no per-principal scoping yet, and half-implementing
one into a published surface was judged worse than not having it).
