# ADR 0014 — Hardening tranche 2: transaction ownership, fenced terminal writes, the multi-pass stream rule, serving bounds

**Status:** accepted, implemented (2026-09-02)
**Contract impact:** `CONTRACT_VERSION` `0.4.0` → `0.5.0`. Every DTO and
vocabulary change here is **additive** — six new `run.warning` codes, optional
`pass`/`reason` on warning data, the synthetic `finishReason: "incomplete"`,
`AiToolResult.data` becoming optional, `run.failed.errorCode` populated where it
was empty, an optional `ProposalDto.claimedAt` — so nothing a `0.4.0` consumer
reads stops parsing. What earns the minor is **behaviour**: a concurrent submit
is refused (`chat_busy`, 409) by default, an SSE run stream closes on the TASK
terminal rather than the first run-terminal event, JSON Schema `format` keywords
are validated instead of silently ignored, REST bodies are capped, and a
replayed `Idempotency-Key` with a different body is a 422. The sqlite reference
adapter's schema goes `7` → `8` with no migration path, by the same
recreate-the-dev-database policy every earlier bump used.

## Problem

`0.4.0` ([ADR 0008](0008-distribution-and-adapters-as-products.md), [ADR
0010](0010-chat-lifecycle-search-import.md)–[ADR
0013](0013-serving-surfaces.md)) landed the surface a third-party host actually
embeds — adapters as products, the management REST surface, tool governance,
the correction harness, and three serving packages — in a short run of waves.
Before handing that surface to the two migrations it was built for
([`docs/migration/openpcb.md`](../migration/openpcb.md),
[`docs/migration/onemind.md`](../migration/onemind.md)), it was reviewed by
**six fresh-context adversarial reviewers**: five over core,
transport/client/react, host, durability, and contracts/testing/packaging, plus
a sixth over the two MCP packages. Between them they found **two CRITICAL and
roughly twenty HIGH** defects, and they were not spread evenly across the
codebase — they clustered in exactly the seams a day-one migration walks
through.

The two CRITICALs:

1. **`SqliteConnection.withAsyncTx` flattened ANY caller into an open
   transaction.** The gate was a `txDepth` counter, which answers "is *someone's*
   transaction open" — not "is it *mine*". An unrelated store call that awaited
   while another caller's `transaction()` was in flight silently joined it. Two
   consequences, both silent: an acknowledged write could be rolled back by a
   stranger's failure, and two workers could come out of `claimNext` holding the
   same task. This is the same *shape* as the double-claim [ADR
   0006](0006-hardening-tranche.md) fixed, one level up: 0006 made the depth
   counter exception-safe, which is orthogonal to it being the wrong question.
2. **A write-policy grant could be redirected.** `POST
   /v1/chats/:chatId/write-policy/allowances` spread the request body over the
   grant, and the body's own `chatId` won over the path's. [ADR
   0013](0013-serving-surfaces.md) had just moved those routes under the chat
   *precisely* so an `AuthorizationPort` — which is handed the path and the URL,
   never the body — could authorize them per chat; the spread handed that
   authorization decision back to the caller.

## Evidence

- The reviews traced the two CRITICALs to reproductions, not readings. Both are
  now regression tests that fail on the old code: the transaction gate through
  the shared conformance suite
  ([`packages/testing/src/store-conformance.ts`](../../packages/testing/src/store-conformance.ts),
  which drives both reference adapters), the write-policy override through
  `packages/transport-http/tests/`.
- The chat loop's cancellation, tool-call assembly and serialization paths were
  traced against real provider behaviour rather than the spec: a cancel landing
  on a chunk boundary committed a half answer as `completed`; two tool calls
  sharing an id merged into one; a `BigInt` in a tool result escaped the
  generator with no terminal event at all.
- The host's crash-recovery chain was traced through the message tree: attempt 2
  appended from the placeholder, and a chain append under a parent that already
  has an active child lands `active: false` — so attempt 2's entire turn went
  onto a dead branch while the conversation replayed attempt 1's unanswered tool
  calls forever. The end-to-end reproduction is `e2e vertical slice (D)` in
  [`packages/runner-local/tests/e2e-vertical-slice.test.ts`](../../packages/runner-local/tests/e2e-vertical-slice.test.ts),
  which kills a worker after the internal assistant record has landed, recovers
  onto attempt 2, and proves the zombie cannot land a terminal. It proved its
  own fences by having each one patched out in turn.
- The "one terminal event ends the run" assumption was found in **every** serving
  surface at once — the SSE handler, the client's `streamRun`, and the React
  hooks — which is what identified it as a contract defect rather than three
  client bugs.
- A verifier measured Ajv compile cost at **3.4× per call** without a validator
  cache, which is what turned "add an LRU" from a guess into a decision.
- `scripts/bench-projection.ts` measured the placeholder write path: 2000 deltas
  produced 2004 `updateMessage` calls in 201.8 ms.

## Decision

### 1. Transaction reentrancy by owner token and a FIFO gate, not a depth counter

`SqliteConnection` gates on an **owner token**, not a counter.
`withAsyncTx(fn, owner)` mints an opaque `TxOwner` when it opens a transaction
and hands it to the callback; a nested call carrying that same owner flattens,
and every other caller queues on a FIFO. `transaction()` hands its callback a
`tx` **view** whose sub-stores carry the owner, so only *its* nested calls
flatten in. Root-level synchronous writes go through `whenFree(fn, owner)`,
which waits out a transaction it does not own.

Rejected: a `txDepth` counter — the defect is not that the counter was
mismaintained but that "someone's transaction is open" is not the same
proposition as "mine is". Also rejected: splitting the gate into `await ready()`
followed by `withTx`. That reads better and is wrong — the two steps leave a
microtask gap in which a transaction already queued on the same gate runs its
own `BEGIN`, which is exactly the flatten being prevented. The gate check and
the `BEGIN` happen in one continuation, and a test pins it.

Waits are **bounded**. `transactionGateTimeoutMs` (default `30_000`; a
non-finite or non-positive value is the documented opt-out to an unbounded wait)
rejects with a typed `TransactionGateTimeoutError` (code
`transaction_gate_timeout`, mapped to **500** — a host-side fault, nothing about
the request was wrong). A silent hang is not an acceptable failure mode to ship
to a third-party host: the shape that produces one is a `transaction()` callback
awaiting a root-store call, which queues behind the transaction it is running
inside, and the error's stack points straight at that callback. A timed-out
caller is rejected but its place in the FIFO **stays** and cancels itself when
its turn comes; dropping the entry would settle the promise the next caller is
already chained to while the transaction is still open.

The memory adapter mints owners the same way and gained the same typed timeout
(its nested `tx.transaction` used to hang outright). It does **not** queue
ordinary writes behind an open transaction — recorded as an adapter-**MAY** in
[`docs/ports.md`](../ports.md), because memory has no rollback, so the failure
mode the queueing prevents does not exist there.

Files:
[`packages/adapters-sqlite/src/sqlite-assistant-store.ts`](../../packages/adapters-sqlite/src/sqlite-assistant-store.ts),
[`packages/adapters-memory/src/memory-assistant-store.ts`](../../packages/adapters-memory/src/memory-assistant-store.ts),
[`docs/ports.md`](../ports.md).

### 2. Fenced terminal writes

`transitionTask`, `endAttempt` and `markDeadLettered` take an optional
`{ leaseToken }` (`FencedWriteOptions`). When one is given the store MUST verify
**inside the same synchronous body as the write** that it names the task's
current lease, and reject a stale one with `LeaseLostError`. `appendEvents` and
`updateProgress` had always demanded a token; the *terminal* writes had not, and
`Lease.fencingToken` was a field nothing ever compared — so a zombie attempt
(lease expired mid-tool-call, recovery already running attempt 2) landed the task
and ended its own attempt anyway, burying the live attempt's verdict.

`TurnRunner`'s terminal block is **ordered around the fence**: fenced
`transitionTask` → `endAttempt` → only then the placeholder finalize. That order
is the whole mechanism for the last write, because `ConversationStore` is
lease-unaware and cannot refuse a zombie on its own; putting the fenced
transition first makes the unfenced write unreachable for an attempt that has
lost the task.

Fencing is **optional per call**, deliberately. A host that lands a task from
outside any lease — a cancel from an HTTP handler, a boot-time reaper,
`TaskService` — has no token to offer, and the recovery paths that repair a
crashed run write *after* the lease is gone by design. Making it mandatory would
break the paths that exist to clean up after the paths it protects.

Runner supersede now **aborts** the old execution. The earlier attempt at this
skipped `this.active` instead, which broke crash recovery outright; abort is
what supersede has to mean.

`TaskRecord.poisonCount` is now incremented **by the store**, on
`endAttempt({ status: "abandoned" })`, rather than patched by the runner at
dead-letter time. An abandoned attempt *is* the poison event, so the count moves
to where that event is written: it is exact after every recovery instead of only
the last one, it cannot be lost to a crash between the attempt write and a later
transition, and two recoverers cannot read-then-write over each other. It is
**idempotent per attempt** — ending the same already-abandoned attempt a second
time reports one death, not two. `TaskPatch.poisonCount` remains, as an override
for a host that is reconstructing history.

### 3. A crashed attempt continues attempt 1's chain

Recovery resumes from `ConversationStore.lastMessageOfRun`, not from the
placeholder. Appending from the placeholder put attempt 2 off-path for the
structural reason in Evidence: the placeholder already has an active child
(attempt 1's internal assistant record), and a chain append under a parent with
an active child lands inactive.

A throw mid-turn lands `run.failed` (or `run.cancelled`) on the durable log
**before** the fenced transition, and finalizes the placeholder only if **this**
attempt is the one that landed the task — otherwise the throw path is exactly
the zombie the fence just refused. `availableAt` is normalized to a UTC ISO
instant on both `CreateTaskInput` and the `TaskPatch` a backoff writes, and an
unparsable value is rejected (`invalid_timestamp`): the adapters compare it as
text or parse it back to a `Date`, so an un-normalized offset form is claimed
hours early on one adapter and not the other, silently, in the retry paths
nobody watches. The outbox is bounded — `maxAttempts` (default 10, after which a
record stays as an inspectable dead letter rather than being redelivered forever)
plus `prune(before)`.

### 4. Submit exclusivity: `chat_busy` by default

`createTask({ exclusiveScope: true })` refuses with `ChatBusyError`
(`chat_busy`, HTTP 409) when any task in the scope is not terminal — checked as
the first statement of the **same adapter transaction** as the insert, with no
`await` between them, because two racing submits is the case the flag exists for
and a caller's own pre-check cannot be atomic. Both reference adapters raise it
through the shared `assertScopeIdle`, so a caller cannot tell which store
refused. A duplicate `taskId` still wins: a redelivered submit is answered as
the retry it is, not refused as busy.

`TurnRunner` passes the flag on **every** submit and regenerate. Default-on,
because users type while the model is generating and this is not hypothetical:
the second turn's user message takes the active-leaf slot under the live run's
internal records, and the chain corrupts. A host that genuinely wants concurrent
turns per chat opts out with `TurnRunnerDeps.allowConcurrentSubmit`.

Rejected: leaving it opt-in. An opt-in guard against a default-path corruption
is a guard nobody has turned on when it matters.

### 5. A run is not one pass — and that is a contract, not a client detail

`TurnRunner` may drive `runChat()` several times under one task id, and each
pass writes its own `run.started` … terminal pair onto the same log. The host
writes `run.warning { code: "retry_pass", pass, reason }` **immediately before**
every recovery pass (`chat_only`, `empty_response`) and every correction pass
(`correction`), so a consumer can tell "the previous pass ended" from "the run
ended".

Everything downstream folds on that boundary:

- `@agentkit/transport-http`'s SSE handler closes the stream on the **TASK**
  terminal, not the first run-terminal event: it re-reads the task status
  immediately after a run-terminal event and drains the tail fully before
  closing.
- `@agentkit/client` no longer stops on the first terminal; `runPhase()` /
  `createRunPhaseTracker` fold the log so the **last** terminal wins, and a pass
  boundary clears it.
- `@agentkit/react` resets the streamed text and the status at a boundary,
  mirroring the host's own reset of the stored answer.

Rejected: first-terminal-closes-stream, i.e. treating it as three independent
client bugs. Every serving surface had written the same assumption
independently, which is the signature of a missing rule in the contract rather
than of three mistakes — and a fourth surface would have written it a fourth
time. It is documented in `AiRunWarningCode`'s own doc comment and in
[`docs/contracts.md`](../contracts.md), so a host writing its own client reads
the rule before it writes the bug.

### 6. Host hooks run under deadlines

`ContextProvider`, `AttachmentResolver` and `ToolSetContributor.contribute` run
under `withHookDeadline`
([`packages/host/src/turn/hook-deadline.ts`](../../packages/host/src/turn/hook-deadline.ts)),
as does the verifier. `DEFAULT_HOOK_TIMEOUTS_MS`, overridable per host through
`TurnRunnerDeps.hookTimeoutsMs`:

| Hook | Default |
| --- | --- |
| `verify` | 30 s |
| `context` | 10 s |
| `attachments` | 10 s |
| `contribute` | 15 s |

A timeout **degrades** the turn rather than failing it — no bindings and no
system prompt, or one contributor's tools missing — and writes `hook_timeout`
onto the durable log. The two exceptions are deliberate: a resolver timeout
reports `attachment_unresolved`, because the dropped part is the outcome a
consumer acts on, and the single-shot verifier still fails the turn, which is
what it always did when `verify()` threw — now bounded instead of forever.

A deadline is a **race, not a cancellation**. Nothing here can stop host code
that is not watching a signal, so a late hook is not cancelled; its answer is
discarded when it arrives. Putting an `AbortSignal` on the hook ports would make
real cancellation possible and is out of scope (below) — it is a port-surface
change, and the bound is the part that stops a run hanging forever.

### 7. Proposals and allowances

Write-policy allowances are scoped by `scopeKey` as well as
`(chatId, toolName, proposalKind)`, so a grant for one design does not
auto-apply a write to another; a grant recorded **without** a `scopeKey` still
matches any scope, which is what every grant made before this existed meant.
Model-facing strings on the proposal path are fixed, not composed from caller
input.

`ProposalRecord.claimedAt` is new: it is stamped on the `approved → applying`
claim, and `reconcileInterrupted({ staleAfterMs })` keys its staleness window on
it. It used to fall back to the decision time, which can be arbitrarily older
than the claim — a proposal approved on Monday and claimed on Friday was
"stale" the instant it was claimed, so the reconciler could take a live apply
away from the worker running it. The window is meant to measure *how long this
apply has been in flight*, which is only what `claimedAt` says.

`ProposalDto.claimedAt` was added (optional, additive) and threaded through the
transport projection. `RunDto` deliberately still omits `poisonCount`, with the
rest of the queue bookkeeping: it exists so the system can recover from a crash,
and publishing it would freeze private mechanics into a public contract.

`idx_messages_run` is `messages(chat_id, run_id, depth, order_key)` — the whole
`ORDER BY` of `lastMessageOfRun`, not a prefix of it, so `EXPLAIN QUERY PLAN`
shows no temporary b-tree and the hot recovery read does not sort the message
table each time.

### 8. The chat loop tells the truth

- **Cancellation is re-checked per chunk.** A cancel landing on a chunk boundary
  used to commit a half answer as `completed`.
- **Tool-call accumulators are keyed index-primary, id-secondary.** Pure id
  keying merged two separately-indexed calls that happened to share an id — real
  providers do that. A name change on the same index splits the accumulator
  rather than blending two calls.
- **Duplicate ids are re-keyed `<id>#2`, `<id>#3`** in the provider client *and*
  again in the loop, with a `duplicate_tool_call_id` warning, so every call keeps
  a distinct, answerable `tool_call_id`.
- **`stream_incomplete` + `finishReason: "incomplete"`.** A stream that ends
  without `[DONE]` and without a `finish_reason` reports the synthetic
  `"incomplete"` rather than defaulting to `"stop"` — that default is precisely
  the lie that made a half answer look final.
- **`safeStringify` → `result_unserializable`.** A `BigInt` or a cycle in a tool
  result used to throw straight out of the generator, after the tool's side
  effects were real and with no terminal event for the run. The tool ran; only
  the reporting failed, and that is what the model is told.
- **Tool deadlines are a real `Promise.race`** (`timeoutMs` used to be
  advisory), with a loop-wide `defaultToolTimeoutMs` for tools that declare
  none, and a late result dropped rather than surfacing as an unhandled
  rejection.
- **The model-facing envelope is capped at `limits.maxBytes` including
  `summary`, `warnings` and error text**, with a last-resort backstop — the text
  fields were the way past a cap that only measured `data`.
- **Ajv per tool with `ajv-formats`**, `$id`/`$schema` stripped **recursively**,
  node and depth caps, a typed `ToolSchemaError`, and a validator **LRU** (512
  entries, keyed by the stripped schema JSON) after the 3.4× measurement above.
  This is a **behaviour change**: `format` keywords are now validated, where
  before an `email` or `uri` constraint in a tool schema was decoration.
- **Provider client**: SSE framing per spec, abort throws rather than returning,
  `reader.cancel()` on the way out, a 1 MiB cap on a parser buffer that has seen
  no frame boundary (`sse_parse`, refusing to buffer more), error bodies bounded
  at 64 KiB on every path including a body-less response, and an `errorCode` on
  **every** `run.failed` it emits — `network_error`, `empty_body`,
  `provider_error`, or the HTTP status as a string.

### 9. Transport bounds

`maxBodyBytes` defaults to 1 MiB (**413** `body_too_large`) and JSON nesting is
capped at depth 64 (**400** `invalid_body`), checked after the parse rather than
during it because the walk is cheap next to the parse that already happened. The
tool-events walk is bounded. Idempotency compares a **body fingerprint**: the
same `Idempotency-Key` replayed with a different body is **422**
`idempotency_key_mismatch` — 422 and not 409 because nothing about the record
moved and retrying is not what the client should do. That check is
transport-level, over a `listSiblings` read, with no host change: the host's
idempotency is `taskId`, and this is the layer that knows what bytes were sent.

The provider id grammar (`^[A-Za-z0-9._-]{1,64}$`, and not only dots) is
enforced at create **and** where the secret ref is minted (`planApiKey`) — an id
that reaches the `SecretStore` unvalidated is a path-shaped string in a key
namespace. MCP server config rows are validated for alias and resilience bounds
at the REST boundary, and the alias grammar (`^[a-z][a-z0-9-]*$`) is **restated
byte-identically** in
[`packages/transport-http/src/validate.ts`](../../packages/transport-http/src/validate.ts)
rather than imported: a sideways import of `normalizeServerAlias` from
`@agentkit/mcp-client` was rejected as a layering violation (the transport does
not depend on an optional adapter beside it), and the restatement is what makes
the rule enforceable *when the row is written* instead of at
`McpClientManager` construction, where one bad row stops every chat while the
host is being wired. Every response of status ≥ 500 carries a generic detail.

`IMAGE_URL_PATTERN` constrains an image part's `url` to `^https?://`, which
closes `file:`/`data:`/`gopher:` at the schema. It does **not** close
private-range SSRF: an `https://10.0.0.1/...` url is well-formed by this rule.
Egress policy is the host's, and saying so here is the point — a consumer
should not read the pattern as a network guard.

### 10. MCP caps

`@agentkit/mcp-server` defaults
([`packages/mcp-server/src/types.ts`](../../packages/mcp-server/src/types.ts)):

| Bound | Default |
| --- | --- |
| `maxRequestBytes` | 4 MiB (413) |
| `maxBatchSize` | 8 (`-32600`) |
| `maxConcurrentCallsPerSession` | 4 |
| `maxCallMs` | 120 s |
| `maxSessions` | 64 |
| session idle TTL | 30 min |

Eviction is per-fingerprint first with a global backstop, so one noisy principal
cannot evict everyone else's sessions to make room for its own; when nothing is
evictable the answer is **503 with `Retry-After`** rather than a refusal with no
guidance. Reaping is in-flight-safe — a session with a call running is not
collected out from under it. `principal` is threaded through to the tool guards,
so a guard can decide on *who is calling*, which is the whole point of [ADR
0013](0013-serving-surfaces.md)'s session-to-principal binding being available
below the transport.

`@agentkit/mcp-client`: `close()` awaits an in-flight connect **and** an
in-flight reconnect before disposing; the private `open()` never clears
`disposed`, so only an explicit `connect()` revives a closed session (a
reconnect triggered by a failing `tools/call` used to); `withDeadline` races for
real; identities are zipped **by position** against the tools they describe; and
result text is capped.

### 11. Client and React

Resumed streams are deduped by **`seq`**, not by an event-id window — the window
collapsed on logs longer than 4096 events, which is a long chat, not a
pathological one. Retry delay is clamped to [250 ms, 30 s] (a server hint of `0`
is a reconnect loop with no pause in it; a hint of an hour is a run the UI never
recovers) with a total reconnect budget of 50 that progress earns back — the
failure being bounded is a connection that never delivers, not a long run that
drops twice. `@agentkit/react` aborts and resets on a chat switch, rolls back
optimistic writes **by id** rather than by count, and restores a truncated tail
when a branch submit fails.

Two findings from this wave are recorded as **known and deliberately not fixed
here**:

- `TurnRunner.runTurn`'s terminal block is fenced `transitionTask` → fenced
  `endAttempt` → **unfenced** `updateMessage`. A lease that moves between the
  first two awaits leaves the task terminal with the placeholder still
  `placeholder: true`. It is unreachable in a single process (nothing else holds
  that lease in the window) and closing it properly means a lease-aware
  `ConversationStore` write, which is a port change.
- `SingleProcessTaskRunner.stillHoldsLease` probes ownership by **renewing**, so
  the settle path extends the lease as a side effect of asking whether it holds
  it. Harmless where it is used — the settle immediately follows — but it is a
  read that writes.

### 12. Projection

An early `run.tool.requested` (before the assistant record exists) is buffered
and applied when the record lands, instead of persisting an assistant record
with no tool calls on it. A failing `usage.record` is guarded and logged rather
than taking the turn down with it. Streamed deltas are **coalesced** into the
stored placeholder: every `run.message.delta` still lands on the durable log, in
order, before anything else — that is what a consumer follows and it is
unchanged — but the placeholder behind it is a projection of that log and does
not need one write per delta. Measured with
[`scripts/bench-projection.ts`](../../scripts/bench-projection.ts): 2000 deltas
went from 2004 `updateMessage` calls / 201.8 ms to 63 calls / 5.2 ms.

### 13. The correction harness shows the verifier the user request

`CorrectionConfig.includeUserRequest` defaults to **`true`**. The minimal
re-context sends the system prompt, the previous answer and the write-back,
which leaves the model correcting work without knowing what was asked for: "add
the decoupling capacitors" and "add the decoupling capacitors to U3 only"
produce the same previous answer and the same deficiency list, and a model that
cannot see which one it was asked can only guess.

**This diverges from OpenPCB's harness**, which did not show the verifier pass
the user request. Stated plainly because the migration playbook's parity claim
depends on it: an OpenPCB migration that wants byte-parity with the old
behaviour must set `includeUserRequest: false` explicitly — it is `false`, not
absent, that turns it off. Off is the right setting for a host whose deficiency
lines are already self-contained and whose requests are long enough that
replaying one is a real cost.

### 14. Testing and packaging

The golden traces are **replayed live** for every scenario — they had been
vacuous — and their drivers live in
[`packages/testing/tests/golden-scenarios.ts`](../../packages/testing/tests/golden-scenarios.ts),
**not** in `src/`. That is the testing package's standing invariant:
`@agentkit/core` is a **peer** it never imports at runtime from `src/`, and a
driver that constructs a run loop is runtime core usage. Four new Phase-2
scenarios cover the loop's newly-honest paths — `tool-cap-run`,
`duplicate-id-run`, `tool-timeout-run`, `unserializable-run`. There is
deliberately **no** golden for `finishReason: "incomplete"`: it is produced by
the SSE client from a stream that ends early, which a recorded trace cannot
express.

Also: `describeSecretStoreConformance` plus a reference `MemorySecretStore` to
grade against it; a **nightly three-seed random matrix** over the durability
schedule driver, on fresh run-specific seeds, beside CI's fixed `[1, 7, 1337]`;
one `it.skip` remaining, holding the real two-handle
claim-and-execute repro [ADR 0006](0006-hardening-tranche.md) left open; a CI
dist guard proving every shippable dist exists and imports nothing from `bun:`
(with `adapters-sqlite` excepted by design); the umbrella package gaining a root
`"."` export of contracts and `sideEffects: false` everywhere;
`AiToolResultSchema.data` becoming optional; and a schema compile test under
`strict: true`.

### 15. sqlite schema 8

`SCHEMA_VERSION` `7` → `8`
([`packages/adapters-sqlite/src/schema.ts`](../../packages/adapters-sqlite/src/schema.ts)):
`idx_messages_run ON messages(chat_id, run_id, depth, order_key)` and
`proposals.claimed_at`. **No migrations, by design** — the store applies its own
DDL on a fresh database, guards the file with `PRAGMA user_version`, and refuses
one written by a different version. A v7 development database is deleted and
recreated, not upgraded.

## Alternatives considered

- **Keep a `txDepth` counter and make it more careful.** Rejected: the counter
  answers the wrong question. No amount of care makes "a transaction is open"
  mean "my transaction is open".
- **Split the gate into `await ready()` then `withTx`.** Rejected: the microtask
  gap between the two is exactly the window a queued transaction runs its
  `BEGIN` in. Pinned by a test so it does not get "simplified" back.
- **Let the gate wait forever.** Rejected: the shape that produces the wait — a
  transaction callback awaiting a root-store call — parks a request with nothing
  in any log to read. A typed timeout naming the callback is the difference
  between a bug report and a shrug.
- **Make `leaseToken` mandatory on the terminal writes.** Rejected: the recovery
  and cancel paths write without a lease by construction, so mandatory fencing
  would break the cleanup that exists to repair the failures fencing protects
  against.
- **Skip `this.active` on supersede instead of aborting.** Tried, rejected: it
  broke crash recovery, because the skipped entry is what recovery reads.
- **Key tool-call accumulators purely by id.** Rejected: it merges two
  separately-indexed calls that share an id, which real providers emit.
- **Fix "first terminal event closes the stream" in each client.** Rejected: all
  three surfaces had independently written the same assumption, so the rule
  belongs in the contract (`retry_pass` + the task-terminal close rule), not in
  three patches and a fourth one later.
- **Leave `chat_busy` opt-in.** Rejected: the corruption it prevents is on the
  default path, and an opt-in guard is off exactly when it is needed.
- **Import `normalizeServerAlias` from `@agentkit/mcp-client` into the
  transport.** Rejected as a layering violation — the transport would take a
  dependency on an optional adapter beside it. The grammar is restated
  byte-identically instead, with a comment saying why.
- **Put the golden drivers in `packages/testing/src/`.** Rejected: it would make
  `@agentkit/core` a runtime dependency of the testing package, whose whole
  contract is that core is a peer.

## Consequences

A host embedding `0.5.0` now has to handle:

- **409 `chat_busy`** on `submitMessage` and `regenerateMessage` while a turn is
  live in that chat — a real response its UI must render, not an error path. Opt
  out with `TurnRunnerDeps.allowConcurrentSubmit` if it genuinely wants
  concurrent turns.
- **Multi-pass streams.** A terminal run event is not the end of the run. A
  consumer must fold `retry_pass` (treat the run as live again, reset streamed
  text) and wait for the stream to close, which now means the *task* is
  terminal.
- **Hook timeouts.** A `ContextProvider`, `AttachmentResolver` or
  `ToolSetContributor.contribute` that routinely takes longer than its default
  will now degrade turns with `hook_timeout`; raise `hookTimeoutsMs` or make the
  hook faster.
- **Format validation may reject tool schemas that used to pass.** A `format`
  keyword that was decoration is now enforced.
- **1 MiB request bodies and depth-64 JSON** by default. A host that posts large
  inline base64 attachments must raise `maxBodyBytes` or move to `ref` image
  sources.
- **A fresh sqlite database.** A v7 file is refused; delete and recreate the dev
  database.
- **`poisonCount` is exact.** It counts every abandoned attempt, not only the one
  that happened to precede a dead-letter. A host reading it as "how many times
  did the dead-letter path run" will read a larger, more correct number.

## Out of scope (deliberate)

Tracked in [`docs/roadmap.md`](../roadmap.md)'s Later list rather than fixed
here:

- `deleteProvider`'s secret-ordering, which still deletes the config row before
  the secret (the create/update path was already inverted).
- `sse.ts`'s heartbeat reading `Date.now()` directly instead of the injected
  `Clock`, and its `roomAvailable()` having no idle deadline — a reader that
  stops reading without closing the socket parks the pump indefinitely.
- `mcp-server`'s unknown-vs-wrong-principal 404s being distinguishable by
  **timing** (identical bodies, different work done before answering).
- FTS5's `rowid` keying and the `VACUUM` hazard behind it.
- `withHookDeadline` cannot **cancel** a hook, only stop waiting for it — real
  cancellation needs an `AbortSignal` on the hook ports.
- The memory adapter not queueing ordinary writes behind an open transaction
  (adapter-MAY; full parity needs sub-store state injection).
- `useChat.error` staying stale until the reconcile during pass 2, and a failed
  submit clearing the run fields of a concurrently accepted run.
- MCP `toolAliases` **values** not being grammar-checked at the REST boundary
  (the server alias is).
- Duplicate-id re-keying running twice for a first-party run — once in the
  provider client, once in the loop. Harmless (the second pass finds nothing to
  do) and kept because a host layering its own provider adapter gets the loop's
  pass regardless.
- Warnings past the envelope budget being dropped **from the envelope only**;
  they stay on the run's event log.
- The one-write window between a pass's terminal event and the `retry_pass` that
  follows it: they are two separate appends in `turn-runner.ts`, and `runPhase`
  reports `failed` for that instant. Host-side batching of the two appends would
  close it.
- The two findings recorded under Decision 11 (the unfenced placeholder
  finalize; `stillHoldsLease` renewing as it probes).

## Dead ends worth not repeating

- Skipping `this.active` on runner supersede — it breaks crash recovery.
- Splitting the transaction gate into `ready()` + `withTx` — microtask gap.
- Keying tool-call accumulators purely by id — real providers reuse ids across
  indices.
- Putting the golden drivers in `packages/testing/src/` — makes core a runtime
  dependency of the testing package.
- Importing `normalizeServerAlias` sideways into the transport — layering
  violation; restate the grammar.
- `git stash` in a shared worktree — it operates on the whole repository, not
  the worktree, and takes another agent's in-flight work with it.

## Verification

Three fresh-context adversarial verifier waves ran inside this tranche, each
over the diffs of the phases before it: wave 1 (core + MCP; transport, client,
react + packaging) raised 25 findings, wave 2 (durability + host; the multi-pass
stream rule) raised 9 and confirmed the rule on every layer, wave 3 checked the
Phase-6 file splits were pure moves. Every confirmed finding was fixed in the
same session with a regression test that goes red if the fix is reverted. Final
gate: **1610 tests passing, 1 skipped**, across typecheck, lint, build, the
umbrella assembly and both smokes.
