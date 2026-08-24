# @agentkit/transport-http

The optional fetch-standard HTTP + SSE adapter that serves
[`@agentkit/contracts`](../contracts)' REST v1 surface over any host that
implements the [`@agentkit/host`](../host) ports.

Nothing in AgentKit depends on this package, and a host is always free to write
its own transport against the same ports. This one exists so the common case —
an HTTP API that matches the published contract exactly — is not rewritten in
every embedding.

`createRestHandler(deps)` returns a plain `(Request) => Promise<Response>`. No
framework, no server, no router library, and **no npm dependencies at all**:
everything it needs (`Request`/`Response`, `ReadableStream`, `URL`,
`crypto.subtle`, `TextEncoder`) is a web standard available in Bun, Node ≥ 19
and Deno.

## Wiring

```ts
import { serveRest } from "@agentkit/transport-http";
import { ProposalService, TaskService, TurnRunner, defaultClock, defaultIds } from "@agentkit/host";

const deps = {
  store,       // AssistantStore — every read goes through it
  turns: turnRunner,   // TurnRunner: submitMessage
  tasks: taskService,  // TaskService: cancelTask
  proposals: proposalService, // optional; the decision routes 501 without it
  packages: { "@agentkit/host": "0.1.0" }, // optional, reported by GET /v1/version
};

Bun.serve({ port: 3000, ...serveRest(deps) });
```

`serveRest(deps)` is a one-line convenience returning `{ fetch }`. Everything
else — port, TLS, lifecycle, logging middleware — stays the host's, because a
transport package that started owning them would stop being optional.

**Mounting under a framework.** The handler takes a standard `Request`, so any
framework that can hand one over works:

```ts
// Hono
const handler = createRestHandler(deps);
app.all("/v1/*", (c) => handler(c.req.raw));
```

Paths are matched exactly as `REST_ROUTES` declares them (`/v1/...`), so mount
at the root of the origin rather than under a prefix; a prefixed mount would
have to rewrite `req.url` before calling in.

### `deps`

| Field | Required | What it is |
| --- | --- | --- |
| `store` | yes | `AssistantStore` — conversations, tasks, proposals, providers |
| `turns` | yes | Anything with `submitMessage` (i.e. `TurnRunner`) |
| `tasks` | yes | Anything with `cancelTask` (i.e. `TaskService`) |
| `proposals` | no | Anything with `approve`/`reject`/`apply` (i.e. `ProposalService`) |
| `toolCatalog` | no | `() => Promise<AiToolDefinition[]>` backing `GET /v1/tools` |
| `packages` | no | Reported as `VersionDto.packages` |
| `authenticate` | no | `(req) => Promise<unknown \| Response>`; a `Response` short-circuits |
| `logger` | no | `Logger`; 5xx and stream failures are logged |
| `streaming` | no | `{ pollIntervalMs, heartbeatIntervalMs, retryHintMs }` — 150 / 15000 / 2000 |

The three service dependencies are declared as **structural** interfaces naming
only the methods this adapter calls, not as the host's concrete classes: those
classes hold private fields, which TypeScript treats nominally, so typing them
directly would make a test double or a hand-rolled equivalent impossible to
pass.

`authenticate` runs after routing (so a nonsense path still 404s honestly) and
before the first store read. Its non-`Response` return value is an **opaque
principal**: this adapter reads it and threads it nowhere, because the contract
has no per-principal scoping yet and half-implementing one into a published
surface is worse than not having it. A host that needs authorization today
filters in front of the handler.

## Idempotency on `submitMessage`

`POST /v1/chats/:chatId/messages` **requires** an `Idempotency-Key` header;
without one it answers `400` with code `idempotency_key_required`. This is the
one write in the API that creates three records at once (a task, the user
message, the assistant placeholder), and a client retrying a timed-out POST
without a key is indistinguishable from a user who really did send twice.

The key becomes the run id, deterministically:

```
taskId = "task_ik_" + sha256hex(chatId + ":" + idempotencyKey)
```

so the same key resolves to the same run from any process, after any restart,
in any replica. It is passed to `TurnRunner.submitMessage` as its `taskId`,
which is already idempotent per caller-supplied id: the second submit writes
nothing, returns the first submit's ids, and re-pokes the queue (the rescue for
a first submit that committed and then died before it could enqueue).

- First submit → **201** with `SubmitMessageResponse`.
- Replay of the same `(chatId, key)` → **200** with the *identical* body.
- A different key → a different run.

The 201/200 split is decided by reading the task row before submitting. Two
genuinely concurrent first submits can therefore both answer 201 with the same
body — a duplicated status code, never a duplicated turn.

## SSE: `GET /v1/runs/:runId/stream`

Headers are `content-type: text/event-stream` and
`cache-control: no-cache, no-transform` (a proxy that buffers or recompresses an
event stream turns live tokens into one blob at the end). The first frame is the
`retry:` hint. Each event is one frame:

```
id: <eventId>
event: <run.message.delta|…>
data: <the AiRunEvent, verbatim — RunEventFrameDto>
```

**Replay-then-poll on a seq cursor, deliberately not replay-then-subscribe.**
The subscribe design has a gap nobody can close: events appended between the
last row the replay read and the moment the subscription attaches are in
neither, and the bug shows up only under load as one dropped delta in a
thousand. A cursor cannot have that race — "what have I sent?" is a number the
reader owns, and every read is defined relative to it. The cost is one poll
interval of latency; the benefit is that replay, resume and live-follow are one
code path instead of three that have to agree.

**Resume.** `Last-Event-ID` carries an `AiRunEvent.eventId`; the stream finds its
`seq` and starts one past it, so a reconnecting client gets exactly the tail it
missed. An **unknown** id replays the whole log from the start rather than
failing — a client holding an id from another run cannot be resumed from, and a
full replay is the only answer that leaves it consistent (and one a client can
dedupe by `eventId`, which a partial stream is not).

**Closing.** The stream ends when a terminal run event (`run.completed`,
`run.failed`, `run.cancelled`) is emitted — and also when the task itself is
terminal but its log holds no terminal event, which happens to a crashed
attempt or a run cancelled before its worker wrote anything. Without that second
rule such a stream would poll forever against a run that will never speak again.

**Liveness and cancellation.** A `: hb` comment goes out after every
`heartbeatIntervalMs` of idleness, so proxies do not reap a connection that is
legitimately quiet during a long tool call. Aborting the request (or cancelling
the stream) clears the poll timer and closes immediately; the 404 for an unknown
run is decided *before* the stream is created, since a `text/event-stream`
response has no status code left to say "no such run".

## Errors

Every non-2xx body is RFC 7807 `application/problem+json`:

```json
{
  "type": "https://agentkit.dev/problems/not_found",
  "title": "Not Found",
  "status": 404,
  "detail": "Run not found: task_ik_…",
  "instance": "/v1/runs/task_ik_…",
  "code": "not_found"
}
```

`code` is what a client branches on — the host's own stable error code, passed
through untouched. The mapping:

| Code | Status |
| --- | --- |
| `not_found` | 404 |
| `invalid_task_transition`, `invalid_proposal_transition`, `duplicate_task`, `duplicate_action_id`, `revision_conflict`, `lease_lost`, `seq_conflict`, `unknown_dependency` | 409 |
| `invalid_decision`, `invalid_request`, `invalid_body`, `idempotency_key_required` | 400 |
| `method_not_allowed` | 405 (with an `Allow` header) |
| `not_implemented` | 501 |
| anything else | 500, logged, with a generic `detail` |

A known path with the wrong verb is a 405 naming what the path *does* accept,
not a 404 that sends a client hunting for a typo.

## Route notes

The router is compiled from the `REST_ROUTES` table rather than transcribed from
it, and the dispatch table is keyed by `RestOperation` — so a route added to the
contract fails this package's compile until it is served.

- **`GET /v1/chats/:chatId/messages`** — `?limit` (default 100) and `?cursor`.
  `nextCursor` is opaque and is the only handle on position (`MessageDto` omits
  the store's `orderKey`); a cursor this server did not issue is a 400, not a
  silent page one.
- **`GET /v1/chats`** — `?limit`, `?before` (ISO timestamp, keyset paging), as
  `ConversationStore.listChats` defines them. The contract declares no page
  wrapper here, so the body is a bare `ChatDto[]`.
- **`POST /v1/runs/:runId/cancel`** — 202, because cancellation is a *request*: a
  queued run is settled in the store immediately, a running one is asked to stop
  and only its worker can land it. The `RunDto` returned is re-read after the
  call, so it reports what actually happened.
- **`GET /v1/chats/:chatId/tool-events`** — derived, since the host has no
  tool-event table: the chat's messages name the runs, and each run's
  `run.tool.*` events project to one `ToolEventDto` each. One row **per event**,
  not per call — the DTO documents `id` as "the eventId of the originating run
  event" and `status` as its stage, so a call appears as `requested`, `running`,
  `succeeded`. `?limit` returns the most recent N, still oldest-first.
- **`GET /v1/providers`** — projects the safe subset (`id`, `label`, `kind`,
  `baseUrl`, `defaultModel`, `enabled`). The contract declares no provider DTO,
  and `apiKey`, `extraHeaders` and `metadata` (where a host keeps its
  `apiKeySecretRef`) are never published.
- **`GET /v1/tools`** — **501 unless `deps.toolCatalog` is supplied.**
  `ToolSetContributor.contribute` is a per-*run* call taking the chat's bindings,
  limits and scope, and this route names no chat; synthesizing a run context
  would advertise a tool set no actual turn receives.
- **`POST /v1/proposals/:id/{approve,reject,apply}`** — **501 unless
  `deps.proposals` is supplied.** A decision arriving over HTTP is recorded as
  `actor: "user"` and never as `"policy"` (which must carry the `policyId` that
  authorised it). `apply` takes the **client's** `operationId` as the
  idempotency key for the side effect; the route never mints one, because a
  server-minted id would make every retry a fresh apply.

Request bodies are validated structurally against the contract's four request
schemas (required fields present and correctly typed, optional fields typed when
present, unknown members allowed for forward compatibility). The validation is
hand-written rather than schema-driven so this package keeps its zero-dependency
promise — Ajv and TypeBox both belong to *other* packages, and reaching through
a transitive dependency is how a lockfile change becomes someone else's runtime
crash.

## Tests

`bun test` (from this directory, or `bun run test:transport-http` from the repo
root):

- `tests/router.test.ts` — every contract route resolves; params decode; 404 vs
  405 + `Allow`.
- `tests/handler.test.ts` — the routes as a client calls them: idempotent
  submit (201 then 200, identical body), validation, run projection, cancel,
  message paging, credential redaction, the 501s, `authenticate`.
- `tests/sse.test.ts` — replay order, `Last-Event-ID` resume, unknown-id full
  replay, terminal close (with and without a terminal event), heartbeat, abort,
  and following a log still being written.
- `tests/proposals.test.ts` — approve/reject/apply over a real `ProposalService`,
  including apply-replay hitting the applier exactly once.
- `tests/tool-events.test.ts` — the two-step derivation and the slim/full
  payload split.
- `tests/e2e.test.ts` — the whole stack behind a real `Bun.serve` on an
  ephemeral port: submit, stream to the terminal event, read the answer back,
  then resume mid-stream from a `Last-Event-ID`.
