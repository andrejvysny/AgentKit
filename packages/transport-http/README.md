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
  turns: turnRunner,   // TurnRunner: submitMessage + regenerate
  tasks: taskService,  // TaskService: cancelTask
  proposals: proposalService, // optional; the decision routes 501 without it
  conversations: conversationService, // optional; DELETE /v1/chats/:id 501s without it
  secrets: secretStore,       // optional; a write-only provider apiKey 501s without it
  writePolicy: sessionWritePolicy, // optional; the allowance routes 501 without it
  mcpConfigs: mcpServerConfigStore, // optional; /v1/mcp/servers 501s without it
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

Paths are matched exactly as `REST_ROUTES` declares them (`/v1/...`). To serve
them under a prefix, set `basePath` (below) rather than rewriting `req.url`.

### `deps`

| Field | Required | What it is |
| --- | --- | --- |
| `store` | yes | `AssistantStore` — conversations, tasks, proposals, providers, settings |
| `turns` | yes | Anything with `submitMessage` + `regenerate` (i.e. `TurnRunner`) |
| `tasks` | yes | Anything with `cancelTask` (i.e. `TaskService`) |
| `proposals` | no | Anything with `approve`/`reject`/`apply` (i.e. `ProposalService`) |
| `conversations` | no | Anything with `deleteChat` (i.e. `ConversationService`) — backs `DELETE /v1/chats/:chatId` |
| `providerOps` | no | `{ refreshModels(id), testConnection(id) }` — backs the two provider probe routes |
| `secrets` | no | `SecretStore` — where a write-only `apiKey` is stored |
| `writePolicy` | no | `WritePolicy` (e.g. `SessionWritePolicy`) — backs the allowance routes |
| `mcpConfigs` | no | `McpServerConfigStore` (`@agentkit/mcp-client`) — backs `/v1/mcp/servers` |
| `toolCatalog` | no | `ToolCatalog` (`@agentkit/host`) backing `GET /v1/tools` |
| `packages` | no | Reported as `VersionDto.packages` |
| `authenticate` | no | `(req) => Promise<unknown \| Response>`; a `Response` short-circuits |
| `authorize` | no | `AuthorizationPort` — consulted per route; a refusal is 403 |
| `basePath` | no | Mount prefix, e.g. `"/api/agentkit"`; stripped before routing |
| `cors` | no | `{ origins, allowHeaders?, exposeHeaders?, maxAgeSeconds? }` |
| `logger` | no | `Logger`; 5xx and stream failures are logged |
| `streaming` | no | `{ pollIntervalMs, heartbeatIntervalMs, retryHintMs }` — 150 / 15000 / 2000 |

**Every optional dependency answers 501, never 404**, when it is absent: the
route exists in the contract and another deployment of the same version serves
it, so telling a client "no such route" would send it hunting for a spelling
mistake. `providerOps` is optional because this package cannot talk to a
provider — refreshing a catalogue and testing a connection are requests to
someone else's server with a credential injected, and a client for that lives
in the host (the same `providerFactory` its `TurnRunner` uses). `secrets` is
optional because a deployment may legitimately have nowhere to put a key: a
`createProvider`/`updateProvider` request **carrying** an `apiKey` then answers
501 rather than writing the credential into the provider config, and the same
request without one works exactly as it would with a secret store wired.

Every service dependency is declared as a **structural** interface naming only
the methods this adapter calls, not as the host's concrete class: those classes
hold private fields, which TypeScript treats nominally, so typing them directly
would make a test double or a hand-rolled equivalent impossible to pass.
`mcpConfigs` is structural for a second reason — importing
`@agentkit/mcp-client` for its record type would make every host that wires this
transport install the MCP bridge, and its SDK, to serve four CRUD routes it may
not use.

## `authenticate` and `authorize`

The two questions are separate, and so are the hooks.

`authenticate` answers **who is calling**. It runs after routing (so a nonsense
path still 404s honestly) and before the first store read. Returning a
`Response` short-circuits the request with it verbatim — 401, a redirect,
whatever the host's scheme needs. Any other return value is the **principal**.

`authorize` answers **whether that caller may do this**. It is the host's own
`AuthorizationPort` from `@agentkit/host`, consulted once per request after
`authenticate` and before the route handler runs — so a refusal costs no store
read and no host call:

```ts
const deps = {
  /* … */
  authenticate: async (req) => verifyBearer(req) ?? new Response(null, { status: 401 }),
  authorize: {
    async authorize({ subject, action, resource }) {
      if (resource.kind === "chat" && resource.id !== undefined) {
        return { allowed: await mayTouchChat(subject.userId, resource.id) };
      }
      return { allowed: action === "read" };
    },
  },
};
```

- **`subject`** is the principal. An object is passed through untouched (so a
  host already returning `{ userId, roles }` needs no adapter); anything else —
  a bare token or id — arrives as `{ metadata: { principal } }`, because this
  adapter cannot tell a user id from a session id and guessing would make
  `subject.userId` mean three different things.
- **`action`** is `read` for `GET` and `write` for every other method.
  Deliberately coarse: the handler knows the HTTP method and nothing else about
  intent, and a finer verb vocabulary invented here would be one this package
  had to keep in step with the route table forever.
- **`resource`** comes from the table in
  [`src/authorize.ts`](src/authorize.ts), which is
  `satisfies Record<RestOperation, …>` — a route added to `REST_ROUTES` fails
  this package's compile until it is given a resource, the same guarantee the
  dispatch table gives for serving it.

| Route | Resource |
| --- | --- |
| `POST /v1/chats`, `GET /v1/chats` | `{ kind: "chat" }` |
| `GET,PATCH,DELETE /v1/chats/:chatId` | `{ kind: "chat", id: chatId }` |
| `GET,POST /v1/chats/:chatId/messages` | `{ kind: "chat", id: chatId }` |
| `POST /v1/chats/:chatId/messages/:messageId/regenerate` | `{ kind: "chat", id: chatId }` |
| `POST /v1/chats/:chatId/fork` | `{ kind: "chat", id: chatId }` |
| `GET /v1/chats/:chatId/tool-events` | `{ kind: "chat", id: chatId }` |
| `GET /v1/chats/:chatId/proposals` | `{ kind: "chat", id: chatId }` |
| `GET /v1/search` | `{ kind: "chat", id: <?chatId> }`, or `{ kind: "chat" }` unscoped |
| `POST /v1/messages/:messageId/activate` | `{ kind: "message", id: messageId }` |
| `GET /v1/messages/:messageId/siblings` | `{ kind: "message", id: messageId }` |
| `GET /v1/runs/:runId`, `.../stream`, `POST .../cancel` | `{ kind: "run", id: runId }` |
| `POST /v1/proposals/:proposalId/{approve,reject,apply}` | `{ kind: "proposal", id: proposalId }` |
| `GET,POST /v1/providers` | `{ kind: "provider" }` |
| `PATCH,DELETE /v1/providers/:providerId`, `.../models`, `.../models/refresh`, `.../test` | `{ kind: "provider", id: providerId }` |
| `GET,PATCH /v1/settings` | `{ kind: "settings" }` |
| `GET,POST /v1/write-policy/allowances` | `{ kind: "policy" }` |
| `DELETE /v1/write-policy/allowances/:allowanceId` | `{ kind: "policy", id: allowanceId }` |
| `GET,POST /v1/mcp/servers` | `{ kind: "mcp_config" }` |
| `PATCH,DELETE /v1/mcp/servers/:serverId` | `{ kind: "mcp_config", id: serverId }` |
| `GET /v1/tools` | `{ kind: "tools" }` |
| `GET /v1/version` | *never authorized* |

The `kind` is always the kind of the id the **path** carries, not of the thing
the route reads. The places that matters: the `/v1/messages/:messageId` routes
carry no chat id and are `message`, not `chat` with a message id in `id` (an
authorizer looking that up as a chat would deny a legitimate request);
`tool-events` derives its answer from runs but *names* a chat, so it is
authorized as one; and the allowance routes are the **policy**, not the chat a
grant is about — an authorizer handed `{ kind: "chat", id: <an allowance key> }`
would look up a chat that does not exist. A host that scopes on conversations
resolves message → chat itself — it is the only side that can.

Two routes are exceptions to "one id from the path", and both are called out in
`src/authorize.ts`. `regenerateMessage` carries a chat id **and** a message id,
and is authorized as the chat it writes to: it creates a branch and a run there,
and the message id names a position inside the conversation rather than the
thing being touched. `searchMessages` carries no path id at all, and takes its
scope from `?chatId=` — with one it is that chat, without one it is the honest
unscoped `{ kind: "chat" }`, so an authorizer that denies it denies exactly the
cross-conversation search, which is the decision it should be making.

`GET /v1/version` is exempt: it reads two constants and the host's `packages`
map, and it is what a client calls to discover whether it can speak to this
server at all. Gating it would stop a client that fails authorization from even
learning what contract version it failed against.

A refusal is `403` with code `forbidden` and the decision's `reason` as
`detail`. **With `authorize` absent — the default — nothing is checked**: every
routed request proceeds. That is the right default for a single-user desktop
embedding and the wrong one for a multi-tenant service, which must supply the
port (or filter in front of the handler).

Spend control is the *other* port and lives elsewhere: `UsageAuthorizer` is
enforced by `TurnRunner` per provider pass, not by this adapter. Its refusal
surfaces here only as the `usage_denied` → **429** row in the error table below.

## `basePath`

```ts
createRestHandler({ ...deps, basePath: "/api/agentkit" });
// GET /api/agentkit/v1/version → 200
// GET /v1/version              → 404 not_found
```

The value is normalized (a leading slash added, trailing ones removed) and
stripped before routing, so `REST_ROUTES`' `/v1/...` paths match underneath it.
`""`, `"/"` and `undefined` all mean "no prefix" — those are the three ways a
config file spells *unset*, and answering 404 to everything because
`BASE_PATH=""` was exported is a fault that looks like a routing bug.

A request **outside** the prefix gets the ordinary 404 problem, since a request
outside the mount is a request for a route this handler does not serve. The
mount root itself (`/api/agentkit`) 404s too. Problem bodies report the **full**
requested path as `instance`, prefix included — that is what the client saw
itself ask for.

## `cors`

Off by default: with `cors` absent no response carries a CORS header and
`OPTIONS` answers 405, exactly as before the option existed.

```ts
createRestHandler({
  ...deps,
  cors: {
    origins: ["https://app.example.com"], // or "*"
    allowHeaders: ["Content-Type", "Idempotency-Key", "Last-Event-ID", "Authorization"], // the default
    exposeHeaders: [],       // default: none
    maxAgeSeconds: 600,      // default: no Access-Control-Max-Age
  },
});
```

- **Preflight.** `OPTIONS` from a matching origin answers `204` with
  `Access-Control-Allow-Origin`, `Vary: Origin`,
  `Access-Control-Allow-Methods` **from the route table** (the methods that
  exact path serves, not a fixed list), `Access-Control-Allow-Headers`, and
  `Access-Control-Max-Age` when configured. An `OPTIONS` for a path that does
  not exist is a 404.
- **Every other response** — JSON bodies, problem bodies, the SSE stream, and
  the host's own verbatim `authenticate` response — carries
  `Access-Control-Allow-Origin` and `Vary: Origin` when the request's origin
  matches, plus `Access-Control-Expose-Headers` when configured. Without them
  a browser cannot read a 401 or a 404 either, and an error a page cannot see is
  worse than one it can.
- **A non-matching origin is not an error.** The request is served exactly as it
  would be with no CORS configured, and the browser — the only party CORS
  protects — refuses to hand the response to the page. Answering 403 would break
  every non-browser client that happens to send an `Origin` header while
  protecting nobody.
- `*` is sent only when `origins` is `"*"`; a list echoes the matched origin.
  `Vary: Origin` goes on unconditionally, because a cache that stored one
  origin's answer for another is the one CORS bug that survives a fix.
- **`Access-Control-Allow-Credentials` is deliberately not offered.** Cookie-
  credentialled cross-origin requests make CSRF reachable and forbid `*`
  outright; a host that wants them should say so at its own edge rather than get
  them from a transport package's options bag.

## Idempotency on `submitMessage` and `regenerateMessage`

`POST /v1/chats/:chatId/messages` and
`POST /v1/chats/:chatId/messages/:messageId/regenerate` **require** an
`Idempotency-Key` header; without one they answer `400` with code
`idempotency_key_required`. They are the only two routes that do, and they are
the two writes in the API that create a run: a submit creates three records at
once (a task, the user message, the assistant placeholder) and a regenerate
creates two, and a client retrying a timed-out POST without a key is
indistinguishable from a user who really did ask twice.

The key becomes the run id, deterministically:

```
submit:     taskId = "task_ik_" + sha256hex(chatId + ":" + idempotencyKey)
regenerate: taskId = "task_rk_" + sha256hex(chatId + ":" + messageId + ":" + idempotencyKey)
```

The **prefixes** are what keep the two apart, not the hashed string. A caller
that submitted with the key `"<messageId>:<key>"` would otherwise hash to the
same digest as a regenerate of that message and land on a run belonging to a
turn it never made; separating at the id also leaves the submit derivation
byte-identical to what it has always been, so no in-flight key is stranded by a
deploy. The message id is in the regenerate's digest because two regenerates of
two different answers are two different runs even under one client-side key.

Either way the same key resolves to the same run from any process, after any
restart, in any replica. It is passed to `TurnRunner.submitMessage` /
`TurnRunner.regenerate` as its `taskId`, which is already idempotent per
caller-supplied id: the second call writes nothing, returns the first one's
ids, and re-pokes the queue (the rescue for a first submit that committed and
then died before it could enqueue).

- First call → **201** with `SubmitMessageResponse`.
- Replay of the same key → **200** with the *identical* body.
- A different key → a different run.

A regenerate's `SubmitMessageResponse.userMessageId` is the question that was
**already there** — the target's parent — not a message this call created. The
old answer keeps its id and its `branchIndex` and simply stops being active, so
a user who prefers it switches back with `POST /v1/messages/:id/activate`.

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

**Bounded reads and backpressure.** Both the replay read and the pre-stream
`Last-Event-ID` scan page the durable log via `readBatchSize` (default
`256`) instead of reading the whole log per poll. The writer pauses on a
saturated consumer (`CountQueuingStrategy(highWaterMark = readBatchSize)` +
`controller.desiredSize`) and resumes on the stream's own `pull` callback,
not a timer — so a slow reader bounds server memory without rationing replay
to a fixed amount per interval regardless of how fast it actually drains.
Frames are never dropped or reordered; heartbeats are skipped while
saturated. See [ADR 0006](../../docs/adr/0006-hardening-tranche.md).

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
| `chat_busy` | 409 (a run in the chat is still live) |
| `duplicate_provider`, `duplicate_alias` | 409 (a create that would overwrite) |
| `invalid_fork_point`, `invalid_regenerate`, `invalid_decision`, `invalid_request`, `invalid_body`, `idempotency_key_required` | 400 |
| `forbidden` | 403 (`deps.authorize` refused) |
| `method_not_allowed` | 405 (with an `Allow` header) |
| `usage_denied` | 429 (`UsageAuthorizer` refused a provider call) |
| `not_implemented` | 501 |
| anything else (incl. `executor_not_found`) | 500, logged, with a generic `detail` |

The host-`code` → status table is `satisfies Record<HostErrorCode, number>`
— a code added to the closed union without a status here fails
`bun run typecheck` rather than silently falling back to 500 (see [ADR
0006](../../docs/adr/0006-hardening-tranche.md)).

`forbidden`, `not_implemented`, `duplicate_provider` and `duplicate_alias` are
**transport-level** codes: this adapter decides them, no `AgentKitHostError` was
thrown, and inventing one to carry them would put a code in the host's closed
union that the host never throws. The two `duplicate_*` codes are checked
*ahead* of the store — `upsertProvider` would happily replace the row, and an
`McpServerConfigStore`'s own refusal is an `McpError` from a package this
adapter deliberately does not import, so it would arrive as an unrecognized
throw and become a 500. The stores' own checks remain the backstop for the race
between the read and the write.
`usage_denied` is the opposite — a real `UsageDeniedError` from
`@agentkit/host`, and a 429 rather than a 4xx that says stop asking, because
`UsageAuthorizationDecision.retryAfterMs` exists for quotas that refill.

A known path with the wrong verb is a 405 naming what the path *does* accept,
not a 404 that sends a client hunting for a typo.

## Route notes

The router is compiled from the `REST_ROUTES` table rather than transcribed from
it, and the dispatch table is keyed by `RestOperation` — so a route added to the
contract fails this package's compile until it is served.

- **`GET /v1/chats/:chatId/messages`** — `?limit` (default 100) and `?cursor`.
  `nextCursor` is opaque and is the only handle on position (`MessageDto` omits
  the store's `orderKey`); a cursor this server did not issue is a 400, not a
  silent page one. The page covers the chat's **active path**, not every message
  ever written to it — a chat nobody has branched has exactly one path, so this
  is unchanged for a linear conversation. The cursor is a position **within one
  path**: it pages forward correctly for as long as the chat stays on the branch
  it was issued on, but a branch switch (this client's or another's) can make the
  live path one whose positions are all *behind* the cursor, and the next page
  comes back empty rather than reporting the new conversation. A client that can
  switch branches must notice the path changed — compare the last item's `id`
  between reads — and re-list from the start instead of continuing the cursor.
- **`POST /v1/chats/:chatId/messages` with `parentMessageId`** — submits the turn
  as a new branch under that message (edit-and-regenerate) instead of appending
  to the end. A parent that is unknown or in another chat is a 404, checked
  *before* the task row is written for the same reason the chat itself is.
- **`POST /v1/chats/:chatId/fork`** — 201 with the new `ChatDto`. Copies the
  active path up to `fromMessageId` (inclusive) into a fresh chat, flattened:
  new ids, no `runId`, a still-streaming placeholder dropped, replay-only
  (`internal`) records kept. A fork point that is unknown or off the active path
  is **400 `invalid_fork_point`** — the request is answerable and wrong; a source
  chat that does not exist is a 404.
- **`POST /v1/messages/:messageId/activate`** — makes that message's branch the
  active path and answers with the path itself (a `MessagePageDto`, no cursor),
  so a branch switch is one round trip rather than an ack plus a re-read. The
  body is what `ConversationStore.activatePath` returned from inside its own
  transaction, not a `listMessages` after it: a read-back could report a path a
  concurrent append had already moved on from. An unknown message is the store's
  `not_found` → 404, with no pre-flight existence check here.
- **`GET /v1/messages/:messageId/siblings`** — the message's siblings *including
  itself*, `branchIndex` ascending, as a bare `MessageDto[]`. Self-inclusive
  because "which answers exist here, and which am I reading?" is one question.
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
- **`PATCH /v1/chats/:chatId`** — rename, re-tag, archive. `metadata` REPLACES
  the stored bag (the port's rule; a merge makes "unset this flag"
  unexpressible). Archiving hides the chat from the default `listChats` query
  and changes nothing else about it.
- **`DELETE /v1/chats/:chatId`** — 204, **501 without `deps.conversations`**.
  Routed through `ConversationService`, not `ConversationStore`: a chat's
  remains live in three stores, and the service is what deletes all three in one
  transaction and what refuses with **409 `chat_busy`** while a run in the chat
  is `running` or `waiting_approval`. A delete is not a cancel, so it waits
  rather than force-cancelling.
- **`GET /v1/search`** — `?q=` (required), `?chatId=`, `?limit=`.
  **501 when the store cannot search**: `ConversationStore.searchMessages` is
  optional, and an empty result set would be a lie a client cannot tell from
  "nothing matched". A *missing* `q` is a 400; a *blank* one is not — the port
  promises a query that sanitizes to nothing returns no hits rather than
  raising, because search boxes emit punctuation. Hits are not restricted to
  the active path: a message on an abandoned branch is still something that was
  said in this chat, and is exactly what the search is for.
- **`GET /v1/providers`** — projects the safe subset (`id`, `label`, `kind`,
  `baseUrl`, `defaultModel`, `enabled`, `apiKeySecretRef`). `apiKey`,
  `extraHeaders` and `metadata` are never published; the secret **ref** is
  lifted out of that metadata bag so a UI can answer "is a key configured?"
  without ever holding one.
- **`POST /v1/providers`, `PATCH /v1/providers/:providerId`** — `apiKey` is
  **write-only**: it goes to `deps.secrets` under the deterministic ref
  `provider/<id>/api-key` and the ref is what the config carries (under
  `PROVIDER_SECRET_REF_KEY`, the same key `TurnRunner` resolves it by). The key
  never appears in a response, in the stored config, or in a log. Without a
  secret store, a request carrying one is a **501** rather than a config with a
  live credential in it. A create over an existing id is **409
  `duplicate_provider`**, never an overwrite; a `PATCH` that replaces
  `metadata` re-folds the ref in, so clearing your own tags cannot silently
  unlink the credential.
- **`DELETE /v1/providers/:providerId`** — 204, and the stored credential goes
  with it: a live key under a ref nothing points at any more is unreachable
  through the API and impossible to notice.
- **`POST /v1/providers/:id/models/refresh` and `/test`** — **501 without
  `deps.providerOps`.** Both talk to someone else's server, which this package
  cannot do. A failed probe is a **200** with `{ ok: false, error }`: the
  request succeeded and the result is the answer, and a 4xx would make "the
  endpoint is down" look like a malformed request.
- **`GET,PATCH /v1/settings`** — the single row, patched partially. The three
  closed unions (`contextSizePreference`, `writePolicyMode`, `toolCalling`) are
  validated rather than passed through: a `writePolicyMode` nothing matches
  would be stored and then silently confirm every write forever.
- **`/v1/write-policy/allowances`** — **501 without `deps.writePolicy`.**
  `?chatId=` is **required** on the listing and the revoke, because the port is
  chat-scoped (see the `deps` section). A grant is always **201** — `allow` is
  an upsert keyed on `(chat, tool, kind)`, so re-granting changes the ceiling
  rather than creating a second grant, and there is no side effect to duplicate.
  A revoke is **204** whether or not the key was there, and a revoke naming the
  wrong chat does nothing.
- **`/v1/mcp/servers`** — **501 without `deps.mcpConfigs`.** `alias` is unique
  (**409 `duplicate_alias`**) because it is the tool namespace every canonical
  id embeds; re-stating a record's own alias in a PATCH is not a collision. The
  transport union is closed, so an unknown `kind` is a 400. `secretRefs` is
  published whole — the record carries `SecretStore` refs, never values, which
  is exactly why it can be.
- **`GET /v1/tools`** — **501 unless `deps.toolCatalog` is supplied.**
  `ToolSetContributor.contribute` is a per-*run* call taking the chat's bindings,
  limits and scope, and this route names no chat; synthesizing a run context
  would advertise a tool set no actual turn receives. The `ToolCatalog` port is
  the honest version of the question: this route calls `listTools()` with **no
  scope**, which is the port's chat-independent set (no bindings, unbound rules).
  A host wires `createContributorToolCatalog({ contributors, context?, guards? })`
  from `@agentkit/host`, which answers by staging the real contributors through
  the real staging path, so the catalogue cannot drift from what a turn gets.
  Entries are `{ namespace, definition }`; the route publishes the `definition`
  only — `namespace` is host-side attribution, not part of `ToolDefinitionDto`.
- **`POST /v1/proposals/:id/{approve,reject,apply}`** — **501 unless
  `deps.proposals` is supplied.** A decision arriving over HTTP is recorded as
  `actor: "user"` and never as `"policy"` (which must carry the `policyId` that
  authorised it). `apply` takes the **client's** `operationId` as the
  idempotency key for the side effect; the route never mints one, because a
  server-minted id would make every retry a fresh apply.

Request bodies are validated structurally against the contract's request schemas
(required fields present and correctly typed, optional fields typed when
present, unknown members allowed for forward compatibility — except the two
CLOSED unions, message content parts and an MCP transport, where an unknown
discriminator is rejected rather than persisted). The validation is
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
- `tests/authorize.test.ts` — the `AuthorizationPort` per route: 403 shape,
  subject/action/resource, the `getVersion` exemption, and a table-driven walk
  of `REST_ROUTES` asserting every route resolves to a resource.
- `tests/mount-cors.test.ts` — `basePath` normalization and routing, and CORS:
  preflight, matched/unmatched origin, and the headers on SSE, on a 404 problem
  and on the host's own `authenticate` response.
- `tests/problem.test.ts` — the two statuses this tranche added (`usage_denied`
  → 429, `forbidden` → 403).
- `tests/branches.test.ts` — the three branching routes: sibling order, the path
  a switch returns, fork 201 vs `invalid_fork_point` 400 vs `not_found` 404, and
  the branching fields on `MessageDto`.
- `tests/regenerate.test.ts` — the regenerate route: the required key, the new
  sibling branch with the old answer left inactive, replay under one key, the
  submit/regenerate key separation, and a real run over the live queue followed
  by a switch back to the first answer.
- `tests/management.test.ts` — the management surface: chat patch/delete
  (including `chat_busy`), search (scoped, unscoped, 501), provider CRUD with
  the write-only `apiKey` asserted against the raw response bytes, settings
  patching and its closed unions, write-policy allowances, and MCP config CRUD
  with the duplicate-alias 409 — plus the 501 for every optional dependency.
- `tests/branches-e2e.test.ts` — over a real socket: ask, ask again, rewrite the
  second question, stream the new branch, switch back, fork the prefix.
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
