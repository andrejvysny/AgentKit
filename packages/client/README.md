# @agentkit/client

The typed client for the AgentKit REST v1 surface — every operation in
[`packages/contracts/src/rest.ts`](../contracts/src/rest.ts) as a method typed
with that contract's own DTOs, plus the two things a hand-rolled client
reliably gets wrong: resuming an SSE run stream across a dropped connection, and
retrying an idempotent write with the key that made it.

It is the mirror of [`@agentkit/transport-http`](../transport-http): that package
serves the contract, this one calls it. Both compile their routing from the same
`REST_ROUTES` table rather than transcribing paths, so a renamed segment breaks
the server and the client together instead of leaving one asking for a URL the
other stopped serving.

**No dependency but `@agentkit/contracts`.** Everything it calls — `fetch`,
`URL`, `ReadableStream`, `TextDecoder`, `crypto` — is standard in browsers,
Node ≥ 20 and Bun. There is no `node:` import anywhere in it, which is what lets
one client serve a web dashboard, an Electron renderer and a CLI without a second
implementation of resume semantics.

## Install

Through the umbrella package:

```jsonc
// package.json
"dependencies": { "agentkit": "github:andrejvysny/AgentKit#v0.5.0" }
```

```ts
import { createAgentKitClient } from "agentkit/client";
```

Or, inside this workspace, `@agentkit/client`.

## Quickstart

```ts
import { createAgentKitClient, runPhase } from "agentkit/client";

const client = createAgentKitClient({
  baseUrl: "http://127.0.0.1:8787",
  headers: async () => ({ authorization: `Bearer ${await accessToken()}` }),
});

const chat = await client.createChat({ title: "First light" });

// The key comes back so a retry of a timed-out submit can reuse it.
const { result, idempotencyKey } = await client.submitMessage(
  { chatId: chat.id },
  { content: "Summarise this board." },
);

let answer = "";
for await (const event of client.streamRun(result.runId)) {
  if (event.type === "run.message.delta") answer += event.data.delta;
  if (event.type === "run.failed") throw new Error(event.data.errorMessage);
}

const page = await client.listMessages({ chatId: chat.id });
```

### Argument shape

`(params, body?, opts?)` — with `params` omitted on the routes whose path and
query define none:

```ts
await client.getChat({ chatId });                       // path params
await client.listMessages({ chatId, limit: 50 });       // path + query
await client.updateChat({ chatId }, { title: "New" });  // params + body
await client.createProvider({ label, kind, baseUrl, defaultModel }); // body only
await client.getSettings();                             // neither
```

Every method takes a final `opts` for a per-call `AbortSignal` and extra headers.

## Streaming and resume

`streamRun(runId, opts?)` is an `AsyncIterable<AiRunEvent>` over the run's
durable log. It is implemented on `fetch` plus a hand-written SSE parser, and
deliberately **not** on `EventSource`: the browser's own SSE client does resume
and backoff for free, but its only configuration is `withCredentials`, so it
cannot carry an `Authorization` header — which every deployment this client is
written for requires.

What it does for you:

| Behaviour | Detail |
| --- | --- |
| **Resume** | On a transport error, it reconnects with `Last-Event-ID` set to the last event it actually yielded. The server replays from **one past** that event, so every event is delivered exactly once and `seq` stays contiguous across the seam. |
| **Backoff** | `retryDelayMs` (default 500 ms) until the server's own `retry:` hint arrives, which then wins. |
| **Retry budget** | `maxRetries` (default 5), and it **resets on every event received** — what it bounds is a server that accepts a connection and immediately drops it, not a long run that drops occasionally. |
| **Heartbeats** | The server's `: hb` comment frames are consumed and ignored. |
| **Abort** | `opts.signal` aborts the request and any pending backoff; no reconnect follows an abort. |
| **Errors** | A problem response (a 404 for an unknown run) throws {@link AgentKitClientError} immediately — it is an answer, not a broken pipe, and will be the same answer on every retry. |

**Iteration ends when the SERVER closes the stream**, which it does when the
**task** is terminal and its log is exhausted — not at a terminal run event. A
run is not one pass: the host re-asks after a failed pass, after a
completed-but-empty one, and once per correction round, and each pass writes its
own `run.started` … terminal pair onto the same log (the `retry_pass` warning
marks the seam). So a break just after `run.failed` is reconnected to like any
other, and `isTerminalRunEvent(event)` tests the end of a *pass*.

A run's own failure is **not** an exception: `run.failed` is yielded like any
other event. An exception from the iterable always means the *call* failed.

### Trailing events, and `drainRun`

The host's correction harness appends `run.verification` events to the log
**after** a pass's terminal event (`TurnRunner`'s base pass emits `run.completed`,
and the harness runs after it). A live stream that is still open sees them, since
the task is `running` throughout — but a stream that was never open at that
moment, or was dropped before it, did not. `drainRun` is the resumed pass that
settles it:

```ts
const events = [];
for await (const event of client.streamRun(runId)) events.push(event);

// One more pass, from where the live stream stopped.
const trailing = await client.drainRun(runId, events.at(-1)?.eventId);
```

It reads once and returns when the server closes — it does not follow, and does
not reconnect. Called without a `lastEventId` it returns the whole log.

## Run phases

`runPhase({ status, events })` collapses a `RunStatusDto` and whatever events a
client has seen into the state a UI actually renders. `streaming` is the phase
the status vocabulary cannot express: a run is `running` from the moment a worker
claims it, whether or not a token has arrived, and the evidence for "typing" is
on the event log.

| Phase | When |
| --- | --- |
| `queued` | `status: "queued"`, or nothing known at all. |
| `running` | `status: "running"` with no `run.started`/delta yet — claimed, not yet answering. |
| `streaming` | Any `run.started` or `run.message.delta` seen, and the run is not terminal. |
| `waiting_approval` | `status: "waiting_approval"` — checked before `streaming`; the user has to act. |
| `completed` / `failed` / `cancelled` | The **last** terminal event, or the matching status. |

A terminal **event** beats the status, because the host appends the event and
*then* transitions the task: a client that read the two in that order holds a
`running` status next to a log that has already ended, and believing the status
would strand a finished run in a spinner.

**Events are read in log order, and the LAST terminal event wins.** A multi-pass
run holds one per pass, and a *pass boundary* after one — a `retry_pass` warning,
or a second `run.started` — clears it, because the run is live again. So a log
ending `run.failed`, `retry_pass`, `run.started`, deltas… is `streaming`, and the
same run reported `failed` only if its final pass failed. `createRunPhaseTracker()`
folds this one event at a time (`observe`, `phase`, and `startedNewPass()` for the
boundary a UI must reset its streamed text on).

Consuming apps' own enums map onto this directly — their `waiting` is `queued`,
their `streaming` is `streaming`, their `paused` is `waiting_approval` — so
migrating means deleting a state machine, not translating one.

## Idempotency

`submitMessage` and `regenerateMessage` are the only routes that require an
`Idempotency-Key`, because they are the only ones that create a run and a message
together: a client that retries a timed-out POST without one asks the same
question twice, and nothing on the server can tell that retry apart from a user
who really did send twice.

Both mint a key when the caller supplies none, and **both return the key they
used**:

```ts
const { result, idempotencyKey } = await client.submitMessage(
  { chatId },
  { content: "hi" },
);

// A timeout, a reload, a retry — the same key lands on the SAME run.
const again = await client.submitMessage(
  { chatId },
  { content: "hi" },
  { idempotencyKey },
);
again.result.runId === result.runId; // true
```

A key supplied through `opts.headers` is ignored: the header is applied after
per-call headers precisely so a stray spread cannot displace it.

## Errors

Every non-2xx becomes an `AgentKitClientError`:

```ts
import { AgentKitClientError } from "agentkit/client";

try {
  await client.getChat({ chatId });
} catch (err) {
  if (err instanceof AgentKitClientError && err.code === "not_found") {
    // `code` is what to branch on — two 409s (`duplicate_alias`,
    // `revision_conflict`) need two different recoveries and the status
    // cannot tell them apart.
  }
}
```

| Member | What |
| --- | --- |
| `status` | HTTP status. |
| `code` | The contract's stable code, or `http_<status>` when the body was not problem+json. |
| `detail` | The problem's human-readable detail, when it carried one. |
| `problem` | The parsed body verbatim, or the raw text when it did not parse. |

A body that is not problem+json — a proxy's HTML 502, a gateway's plain-text 429
— becomes the same error rather than something else: all of them mean "the call
failed", and a client that had to distinguish them before it could show a message
would get it wrong in the one place it matters.

`204` responses resolve to `undefined`.

## Auth

`headers()` is called **per request**, not captured at construction, so a token
that rotates mid-session is re-read rather than remembered. It may be async, so a
caller can refresh before returning:

```ts
const client = createAgentKitClient({
  baseUrl,
  headers: async () => ({ authorization: `Bearer ${await session.token()}` }),
});
```

Per-call `opts.headers` merge over it. The content type and the idempotency key
are applied last — they are decided by the operation, not by the caller.

## What this client does not do

- **It does not validate responses** against the contract's JSON Schemas. The
  DTO types describe what a conforming server sends; checking it would drag
  TypeBox and Ajv into every browser bundle to re-litigate what the server
  already validated on the way out. A client that needs to defend against a
  non-conforming server can compile the schemas from `@agentkit/contracts`
  itself.
- **It holds no cache and no state** beyond the base URL and the header source.
  Two calls are two requests; a store, an optimistic update and a subscription
  model are the consuming app's, and every framework has its own.
- **It does not poll.** `streamRun` follows a run; everything else is a read
  when you ask for one.

## Tests

`bun run test:client` from the repo root. The suite runs the real
`@agentkit/transport-http` handler over the reference in-memory store behind
`Bun.serve` on an ephemeral port — a stub would test the stub. The resume test
supplies its own `fetch` that re-wraps the response body and errors it after a
fixed number of SSE frames, which is what a reset connection looks like to
`fetch`, and asserts the whole log still arrives exactly once across a dozen
severed connections.
