# Changelog

Versions track `CONTRACT_VERSION` (the event/DTO shape version in
`@agentkit/contracts`), not npm releases — nothing is published yet. Each
entry links the architecture decision records (`docs/adr/`) that carry the
full reasoning; this file is the short answer to "what changed and why",
newest first.

## 0.5.0 — 2026-09-02 — Hardening tranche 2: durability fencing, the chat loop's truth, multi-pass streams, serving bounds

**Contract change (minor bump, pre-consumer):** every DTO addition is
optional and every new vocabulary item is additive — six new `run.warning`
codes (`retry_pass`, `stream_incomplete`, `result_unserializable`,
`duplicate_tool_call_id`, `tool_late_result` (reserved), `hook_timeout`),
optional `pass`/`reason` on warning data, the synthetic
`finishReason: "incomplete"` on `run.message.completed`/`run.completed`,
`AiToolResult.data` now optional, `run.failed.errorCode` populated on every
provider-client failure path (`network_error`, `empty_body`,
`provider_error`, the HTTP status), and an optional `ProposalDto.claimedAt`. What earns the minor
is **behaviour**: a concurrent submit into a busy chat is refused with
`chat_busy` (409) by default; an SSE run stream now closes on the *task*
terminal, not the first run-terminal event; JSON Schema `format` keywords are
validated instead of silently ignored; REST bodies are capped at 1 MiB and
depth 64 by default; an `Idempotency-Key` replayed with a different body is
a 422. `CONTRACT_VERSION` `0.4.0` → `0.5.0`; golden traces re-recorded once
(only `contractVersion`/`timestamp`/`eventId` moved), four new golden
scenarios cover the loop's cap/duplicate-id/timeout/unserializable paths.
The umbrella `agentkit` package is versioned `0.5.0` for the first tagged
release (`v0.5.0`, cut per `DEVELOPING.md`).

A six-reviewer adversarial review of 0.4.0 — five fresh-context reviewers
over core, transport/client/react, host, durability and contracts/packaging,
plus a sixth over the MCP packages — found two CRITICAL and roughly twenty
HIGH defects between them, in the places OpenPCB and OneMind would hit on
day one of a migration: the sqlite transaction gate, the write-policy
routes, the chat loop's cancellation and tool-call assembly, the host's
crash-recovery chain, and the "one terminal event closes the stream"
assumption every serving surface shared. Every fix below is pinned by a
test that fails on the old code. The reasoning lives in [ADR
0014](docs/adr/0014-hardening-tranche-2.md).

### Durability: transactions are owned, terminal writes are fenced ([ADR 0014](docs/adr/0014-hardening-tranche-2.md))

- **CRITICAL: sqlite `withAsyncTx` flattened ANY caller into an open
  transaction.** A `txDepth` counter said "someone's transaction is open",
  not "mine", so an unrelated store call awaiting inside another caller's
  `transaction()` silently joined it — an acknowledged write could be rolled
  back by a stranger's failure, and two workers could claim one task. Both
  adapters now gate on an **owner token** with a FIFO queue; `transaction()`
  hands the callback an owner-bearing view (`txView`) so only *its* nested
  calls flatten; root synchronous writes queue behind an open transaction
  (`whenFree`, gate exit and `withTx` in one continuation — a
  `ready()`-then-`withTx` split has a microtask gap, and a test pins it).
  Waits are bounded by `transactionGateTimeoutMs` (30 s) with a typed
  `TransactionGateTimeoutError` (`transaction_gate_timeout`, 500) — a silent
  hang is not an acceptable failure mode for a third-party host. The memory
  adapter mints owners the same way (its nested `tx.transaction` used to
  HANG); it does not queue ordinary writes behind an open transaction, which
  is documented as an adapter-MAY because memory has no rollback.
- **Terminal task writes are fenced.** `transitionTask`, `endAttempt` and
  `markDeadLettered` take an optional `{ leaseToken }` checked inside the
  same synchronous body as the write, and the turn runner lands a pass
  fenced-transition FIRST, then `endAttempt`, then the placeholder — so a
  zombie attempt whose lease expired can no longer overwrite attempt 2's
  answer through the lease-unaware `ConversationStore`. Fencing is optional
  per call because recovery paths write after the lease is gone by design.
  A runner-level end-to-end test now kills a worker after the internal
  assistant record landed, recovers onto attempt 2 on the ACTIVE path, and
  proves the zombie cannot land a terminal.
- **Attempt 2 continues attempt 1's chain.** Recovery used to append from the
  placeholder, landing attempt 2's records on a dead branch (a parent with an
  active child lands new records inactive); the runner now resumes from
  `lastMessageOfRun`. A throw mid-turn lands `run.failed|run.cancelled` on the
  log before the fenced transition and finalizes the placeholder only if THIS
  attempt landed. Runner supersede now ABORTS the old execution instead of
  skipping recovery (skipping broke crash-recovery). `availableAt` is
  normalized (garbage → `invalid_timestamp`); the outbox is bounded
  (`maxAttempts` 10, `prune`).
- **sqlite schema 8** (no migrations by design — recreate the dev database):
  `idx_messages_run` for `lastMessageOfRun`, `proposals.claimed_at`, and
  `TaskRecord.poisonCount` is now incremented by the *store* on
  `endAttempt({ status: "abandoned" })` rather than patched by the runner at
  dead-letter time, so it is exact after every recovery, not only the last
  one, and idempotent per attempt (the same death reported twice counts once;
  `RunDto` still omits the count). `ProposalRecord.claimedAt` is stamped on the
  `approved → applying` claim, and `reconcileInterrupted`'s staleness window
  keys on it (it used to fall back to the decision time, which could be far
  older than the claim).
- **Found by the second verifier wave, fixed the same day**: the FIFO gate
  was not yet fair — a root write already waiting could be overtaken by every
  transaction that arrived after it and starve to the gate timeout; it now
  takes a real slot in the queue. `SqliteMcpServerConfigStore`, which shares
  the database handle, still joined any open transaction and was erased by a
  stranger's rollback; it now queues through a per-handle write gate.
  `endAttempt` keeps an attempt's first terminal status (a recovery pass can
  no longer restate a clean completion as a crash), the local runner lands the
  fenced transition before ending the attempt on every settle path, and
  `openAgentKitDatabase` closes the handle it opened when it refuses a stale
  database.

### The chat loop tells the truth (`@agentkit/core`)

- Cancel on a chunk boundary used to commit a half answer as `completed`; the
  loop re-checks the signal after every chunk. Tool-call deltas are keyed
  index-primary / id-secondary (pure id keying merged two indexed calls that
  shared an id — real providers do that); duplicates are re-keyed `<id>#n` in
  the provider client AND the loop, with a `duplicate_tool_call_id` warning.
  A stream that ends without `[DONE]`/`finish_reason` reports
  `finishReason: "incomplete"` plus `stream_incomplete` instead of claiming
  it finished.
- A tool result that cannot be serialized (a `BigInt`) used to escape the
  generator with no terminal event; `safeStringify` turns it into
  `result_unserializable`. Tool deadlines are a real `Promise.race`
  (`timeoutMs` used to be advisory), with a loop-wide `defaultToolTimeoutMs`.
  The model-facing envelope is capped at `limits.maxBytes` including
  `summary`, `warnings` and error text, with a last-resort backstop.
- **Ajv per tool** with `ajv-formats` — `format` keywords are now
  **validated** (they were silently ignored: an `email` or `uri` constraint
  in a tool schema was decoration) — `$id`/`$schema` stripped recursively,
  node/depth caps, a validator LRU (512, keyed by the stripped schema) after
  a verifier measured 3.4× compile cost per call, and a typed
  `ToolSchemaError`. Provider client: SSE framing per spec, abort throws,
  `reader.cancel()`, a 1 MiB residual cap, bounded error bodies, `errorCode`
  on every `run.failed`.

### Host orchestration (`@agentkit/host`)

- **`chat_busy` by default.** Users type while the model is generating, and a
  second submit corrupted the chain. `createTask({ exclusiveScope })` refuses
  inside the adapter transaction (`ChatBusyError`, 409) on submit and
  regenerate; opt out per host with `TurnRunnerDeps.allowConcurrentSubmit`.
- **A run is not one pass.** The host emits `run.warning { code:
  "retry_pass", pass, reason }` immediately BEFORE every recovery
  (`chat_only`, `empty_response`) or correction pass, so a consumer can tell
  "the previous pass ended" from "the run ended" — the contract every serving
  surface below now relies on. The correction harness gained
  `includeUserRequest` (default **true**: the verifier sees what was asked;
  OpenPCB's harness did not, and ADR 0014 records the divergence).
- Hook deadlines: `ContextProvider`, `AttachmentResolver` and
  `ToolSetContributor.contribute` run under `withHookDeadline` (30/10/10/15 s,
  `TurnRunnerDeps.hookTimeoutsMs`); a late hook degrades the turn with
  `hook_timeout` instead of hanging it. Write-policy allowances are scoped by
  `scopeKey`; model-facing strings are fixed; the emulated-tool-call detector
  requires a staged tool name and understands XML and bare JSON; early
  `run.tool.requested` is buffered in the projection; streamed deltas are
  coalesced into the stored message (2000 deltas: 2004 → 63 `updateMessage`
  calls, 201.8 → 5.2 ms, `scripts/bench-projection.ts`).

### Serving surfaces: transport, client, react, MCP

- **The stream closes on the TASK terminal.** `@agentkit/transport-http`'s
  SSE handler checks the task immediately after a run-terminal event and
  drains the tail fully, so recovery and correction passes reach the client
  on the same stream. `@agentkit/client` no longer stops on the first
  terminal; `runPhase()`/`createRunPhaseTracker` fold the log so the LAST
  terminal wins and a pass boundary clears it; resumed streams are deduped by
  `seq` (an event-id window collapsed on long logs); retry is clamped
  [250 ms, 30 s] with a total reconnect cap. `@agentkit/react` resets the
  streamed text and status at a boundary, takes the last failure, aborts and
  resets on chat switch, rolls back by optimistic ids and restores a
  truncated tail when a branch submit fails.
- **Found by the second verifier wave, fixed the same day**: an SSE pump
  exception (a store hiccup mid-stream) used to end the body *cleanly* — the
  one signal that now means "task terminal" — so the client returned
  mid-pass; it now rejects the stream and the client reconnects. The hooks
  fold the task's status when a stream closes without a terminal run event
  (the host's quiet-failure path writes none), `submit`/`regenerate`/
  `refresh` carry the chat-switch guard `followRun` already had, the
  trailing drain can no longer flip a completed run to an error, a
  `seq`-less event no longer disables replay dedupe, and `finishReason` is
  exposed so an `"incomplete"` answer is not rendered as a finished one.
- **CRITICAL: a write-policy grant could be redirected.** The body's `chatId`
  overrode the authorized path chat on `POST
  /v1/chats/:chatId/write-policy/allowances`; body fields are now named and
  the path wins. Transport bounds: `maxBodyBytes` 1 MiB and JSON depth 64 by
  default, a bounded tool-events walk, an idempotency **body fingerprint**
  (same key + different body → 422 `idempotency_key_mismatch`), a provider id
  grammar enforced at create AND where the secret ref is minted, MCP config
  alias/resilience validation, and a generic detail on every ≥ 500.
- **`@agentkit/mcp-server`**: 4 MiB request cap, batches ≤ 8, 4 concurrent
  calls per session, per-fingerprint session eviction with a global backstop,
  `maxCallMs` 120 s, 503 + `Retry-After` when nothing is evictable,
  in-flight-safe reaping, `principal` threaded to tool guards.
  **`@agentkit/mcp-client`**: `close()` awaits an in-flight connect AND
  reconnect and `open()` never clears `disposed` (a reconnect used to revive a
  closed session), `withDeadline` races for real, identities zipped by
  position, result text capped.

### Testing, CI, packaging

- `describeSecretStoreConformance` + a reference `MemorySecretStore`; the
  golden traces are replayed LIVE for every scenario (they were vacuous) and
  the drivers live in `packages/testing/tests/` so `@agentkit/core` stays a
  peer the testing package never imports at runtime; a nightly three-seed
  random matrix; the one `it.skip` holds the real two-handle repro; the CI
  dist guard proves each dist exists and matches `import "bun:x"`.
- Umbrella: root `"."` export (contracts), `sideEffects: false` on every
  package, `ajv-formats` as a dependency, README install/layering fixes.

### Verification

Three fresh-context adversarial verifier waves ran inside this tranche, each
over the diffs of the phases before it: wave 1 (core + MCP, and
transport/client/react + packaging) found 25 issues, wave 2 (durability +
host, and the multi-pass stream rule) found 9 and confirmed the core rule on
every layer, wave 3 checked the file splits were pure moves. Every confirmed
finding was fixed in the same session with a regression test that fails
(red) if the fix is reverted; the runner-level crash/zombie end-to-end test
proved its own fences by patching each one out in turn. Final gate: 1646
tests passing, 1 skipped, across typecheck, lint, build, the umbrella
assembly and both smokes.

## 0.4.0 — 2026-09-02 — Library-ready: adapters, umbrella, management surface, governance, serving stack

**Contract change (breaking, pre-consumer):** message content widens from
`string` to `string | AiContentPart[]` at persistence (`MessageRecord`,
`SubmitMessageInput`, `MessageDto`, `SubmitMessageRequest`) with a third
image source, `{ kind: "ref" }`; `AttachmentResolver` is a new port. REST v1
grows from 20 to 38 operations (chat update/delete, regenerate, search,
provider/settings/write-policy-allowance/MCP-server-config CRUD); a new
event type, `run.verification` (13th in the vocabulary); two new warning
codes (`attachment_unresolved`, `attachment_budget_exceeded`).
`CONTRACT_VERSION` `0.3.0` →
`0.4.0`; golden traces re-recorded once. Several port signatures changed
pre-consumer, with no external caller yet to break: `AttachmentResolver
.resolve` gained a `{ chatId }` context parameter, the write-policy allowance
routes moved from `/v1/write-policy/allowances` to nest under
`/v1/chats/:chatId/write-policy/allowances` so an `AuthorizationPort` can
authorize them per chat, and `ToolSetContributor.namespace` became required.

This is the wave that turns AgentKit from a well-tested internal framework
into something installable: a single `agentkit` package over a GitHub tag,
the reference adapters promoted from private test scaffolding to production
packages, and the whole surface both of this framework's first two intended
consumers (OpenPCB, OneMind) need to actually migrate onto it — chat
lifecycle, tool governance, a correction harness, an MCP server, and a
shared client + React hooks.

### Distribution: the `agentkit` umbrella, and the adapters as products ([ADR 0008](docs/adr/0008-distribution-and-adapters-as-products.md))

- **One installable package.** `agentkit`, unscoped, installed via
  `"agentkit": "github:andrejvysny/AgentKit#vX.Y.Z"` — no npm publish
  required, no `workspace:*` for a consumer to resolve. Twelve subpath
  exports assembled by a deterministic specifier-rewrite build (no
  bundler); a lockstep release version for the whole package, released by
  subtree-splitting `packages/agentkit`'s committed `dist/` into a
  `release/vX.Y.Z` branch a consumer's tag resolves directly. Chosen over
  per-package tags because a git-tag install has no registry-side version
  enforcement — independent per-package tags would reintroduce the exact
  consumer version-drift class of bug (adapters-sqlite@v0.3.0 against
  host@v0.4.0, with nothing to catch the mismatch) that lockstep exists to
  prevent. Not published to npm: the `@agentkit` scope's ownership is still
  unverified, and GitHub-tag installation is a complete distribution story
  on its own, not a stopgap.
- **`internal/reference-adapters` promoted to `@agentkit/adapters-memory`,
  `@agentkit/adapters-sqlite` (Bun-only), `@agentkit/runner-local`** — a
  stance change, not just a move: sqlite is now documented as **the**
  production `AssistantStore` for a single-process host, owning its own
  database file and shipping no migrations by design (`PRAGMA user_version`
  fails closed on a stale schema). Exponential jittered retry backoff
  (1s→30s, tracked in the runner against an injected `Clock`, never the
  store) and a new `describeTaskRunnerConformance` suite in
  `@agentkit/testing`, run against both stores.
- **Two CRITICAL recovery bugs**, found the moment this became a runnable
  composition root (`examples/desktop-host`) instead of unit-tested
  internals: recovery could strand a task `running` with no lease (fixed
  with a `landed`-gated release plus a `pendingRedispatch` set draining into
  `startWorker`), and a retry could steal a task back from another owner's
  in-flight recovery after a backoff outlasted the lease TTL (fixed with a
  post-backoff `stillHoldsLease` re-check before minting a new attempt).

### Content parts at persistence + `AttachmentResolver` ([ADR 0009](docs/adr/0009-content-parts-attachment-resolver.md))

- `AiContentPart`s reach storage, not just the provider boundary: a message
  body can be text, image parts, or a mix, with a new `ref` image source —
  an opaque handle into the *host's* own blob storage that a new
  `AttachmentResolver` port turns into bytes per provider pass, in memory,
  under budgets borrowed from OpenPCB's `MENTION_LIMITS` (5 MiB/image, 20
  MiB aggregate, 16 images). An unresolvable or over-budget image is
  dropped with a durable warning; the stored message always keeps the ref.
  Search now indexes **all** of a message's text parts, closing a
  first-part-only bug in the reference lineage rather than porting it.
- **Fixed one day later**: `resolve(ref)` widened to `resolve(ref, {
  chatId })` — a ref is untrusted client input, so resolution is an
  authorization question ("may this chat see these bytes"), not a global
  lookup.

### Chat lifecycle, search, transactional import ([ADR 0010](docs/adr/0010-chat-lifecycle-search-import.md))

- `updateChat`/`deleteChat`/`ChatRecord.archived`, `listChats`
  `includeArchived`/`ids`, backward paging (`beforeOrderKey`),
  `importConversation` (the id-preserving, transactional history-migration
  primitive this whole wave exists to unblock), and `searchMessages` (sqlite:
  FTS5 external-content + triggers + `bm25`; memory: substring) behind an
  optional `capabilities.search` flag.
- **Decided, not new code**: the host's canonical status vocabulary
  (`queued|running|waiting_approval|completed|failed|cancelled`) does not
  grow to express consumer UI states — `streaming`/`waiting`/`paused`/
  `pending` are client-derived phases over `(status, event log)`, realized
  later as `@agentkit/client`'s `runPhase()` ([ADR 0013](docs/adr/0013-serving-surfaces.md)).
- **Fixed one day later**: `TaskStore.deleteByScope` now enforces
  `chat_busy` atomically, in one synchronous statement inside the adapter —
  a verifier found that a check made before an `await` and acted on after it
  is not atomic in a single-event-loop host, where a concurrent store call
  flattens into the in-flight transaction rather than opening its own.
  `docs/ports.md` now names this as a hazard class, not a one-off bug.

### Tool governance — P7 ([ADR 0011](docs/adr/0011-tool-governance.md))

- Required `namespace` on every `ToolSetContributor` (`agentkit`/`chat`/`mcp`
  reserved), cross-contributor name collisions fail staging closed, a
  `ToolGuard` chain gating visibility (staging time) and executability (call
  time), contributor `dispose()`, a `toolCalling: auto|on|off` override atop
  a provider's probed capability, structured tool errors
  (`phase`/`retryable`), and a `ToolCatalog` port serving `GET /v1/tools`
  (previously a deliberate 501). This closes the roadmap's P7 gate, unblocking
  the MCP server package, a future tool bridge, and approval workflows.
- **Fixed one day later**: a `ToolGuard` that throws now fails closed
  **per tool**, not per run — an `isVisible` throw hides one tool; a
  `canExecute` throw refuses one call with the fixed reason `"guard error"`
  (never the thrown message, which is not safe to hand the model).

### Multi-pass correction harness ([ADR 0012](docs/adr/0012-multi-pass-correction-harness.md))

- Opt-in `TurnRunnerDeps.correction`: bounded passes (default 3, capped 5),
  minimal re-context (system + last assistant + a deficiency write-back, not
  full history), a strict shrink-or-stall stopping rule, fail-closed on an
  unavailable verifier, durable `run.verification` events, every pass
  usage-authorized like any other. Absent config is byte-identical to the
  existing single-shot behavior. Ports OpenPCB's `runCorrectionHarness`
  semantics (shrink-or-stall, fail-closed, minimal re-context) rather than
  redesigning them.
- **CRITICAL, fixed one day later**: a correction pass reuses the run's own
  id, so a tool-calling correction produced a *second* tool-calling turn on
  one run — which broke the existing per-kind message-ordering scheme
  outright (two `tool_calls` turns back to back, the first left unanswered).
  `orderMessagesForProvider` now groups by tool-call linkage instead of
  record kind, making any number of tool-calling passes on one run
  provider-legal by construction. Correction write-backs are persisted for
  audit but excluded from later provider-history replay.

### Serving surfaces: `mcp-server`, `client`, `react` ([ADR 0013](docs/adr/0013-serving-surfaces.md))

- **`@agentkit/mcp-server`**: exposes a host's `ToolCatalog` as an MCP server
  over streamable HTTP — constant-time bearer auth, a DNS-rebinding origin
  guard, server-minted session ids, write-tool filtering on both `tools/list`
  and `tools/call`.
- **`@agentkit/client`**: a typed method per REST v1 operation
  (compile-exhaustive against the route table), auto-resuming `streamRun`,
  auto-minted `Idempotency-Key`s, `problem+json` as a typed error, and
  `runPhase()` — the client-derived status/phase mapping ADR 0010 decided.
- **`@agentkit/react`**: headless hooks only (`useChat`, `useRun`,
  `useBranches`, `useProposals`, `useProviders`) — no components, no
  styling; `react` is an optional peer. Revises `docs/non-goals.md`'s
  "React / UI packages" entry, narrowed to "no *styled* component library."
- **CRITICAL, fixed one day later**: an MCP session, keyed only on a
  server-minted id, could be reached by any caller who obtained a leaked
  `Mcp-Session-Id` — sessions are now bound to a fingerprint of the
  `Authorization` header that opened them, checked in constant time on every
  request, plus an `maxSessions` LRU cap and idle-TTL reaping.

### Verification

Two independent fresh-context adversarial reviews ran against this wave's
diffs (Phase A, and a combined Phase B/C pass), finding 3 CRITICAL and 9
IMPORTANT issues between them, all fixed in the same session with a
regression test that fails (red) if the fix is reverted, before either wave
was called finished — the recovery-lease and correction-harness ordering
bugs above among them. Final gate: 1352 tests passing, 1 skipped, across the
full `bun run ci`.

## 0.3.0 — 2026-08-25 — Hardening tranche + conversation branching

**Contract change (additive):** `MessageDto` gains
`parentMessageId`/`branchIndex`/`active`; `SubmitMessageRequest` gains
`parentMessageId`; three new REST v1 routes (`forkChat`, `activateBranch`,
`listSiblings`). Golden traces re-recorded; event shapes unchanged.

### Hardening tranche ([ADR 0006](docs/adr/0006-hardening-tranche.md))

A deliberate consolidation pass before more features, prompted by the pace
of the 0.2.0 wave (a shipped concurrency defect was caught by review the
same day it landed — evidence of testing debt at the new boundaries):

- **Durability invariant suite** — seeded schedule driver, fault injection
  at every claim sub-step, and multi-handle SQLite tests (two store
  instances on one file, plus a real two-process contention test). The
  suite found three production bugs, all fixed in the same tranche:
  `withTx` permanently losing atomicity after one `SQLITE_BUSY`; no
  busy-wait strategy for cross-handle contention (fixed with a split
  design — SQLite `busy_timeout` for synchronous transactions, event-loop
  waiting for asynchronous ones); a non-atomic in-memory `claimNext` that
  could strand a `running` task with no lease.
- **SSE hardening** — bounded log reads (`readBatchSize`) on both ends of
  the stream and pull-signalled backpressure (the pump parks on a saturated
  consumer and is woken by the stream's own `pull`).
- **Closed error vocabulary** — `HOST_ERROR_CODES` as an exported union;
  the HTTP adapter's status mapping is compiler-checked exhaustive.
- **CI** — Biome lint/format gate, coverage report (no threshold gate),
  a manual claim-path benchmark with a recorded budget, and a packed-tarball
  install smoke that caught two real packaging bugs (npm does not apply
  `publishConfig.exports` on pack; a JSON import needed an import attribute
  under Node).
- **Runtime policy** — ESM-only, `engines: node >=20 / bun >=1.3`.

### Conversation branching + fork ([ADR 0007](docs/adr/0007-conversation-branching-fork.md))

Message-tree branching (edit-and-regenerate without losing the original)
and whole-chat forking, ported from OneMind's production-proven model and
hardened where the reference was weak: append-and-activate is one atomic
operation, `activatePath` is transactional and tested, and run-produced
records chain onto the run's own branch so a branch switch mid-generation
cannot migrate them. Fork copies the active-path prefix in provider order.
A randomized tree driver guards the single-active-chain invariant in the
conformance suite.

## 0.2.0 — 2026-08-24 — Generic tasks, multimodal, MCP, transport, dependencies

**Contract change (breaking, pre-consumer):** `AiChatMessage.content`
widened to `string | AiContentPart[]`; `TaskEventEnvelope` introduced.

- **Generic task foundation** ([ADR 0001](docs/adr/0001-generic-task-foundation.md)) —
  the chat-specific run model became a kind-dispatched task system:
  `TaskStore`/`TaskRecord { kind, scopeId, payload }`, executor registry,
  `TaskService`, envelope-typed event log. Chat turns are task kind
  `chat.turn`; the chat wire vocabulary (`runId`, REST routes) is unchanged.
- **Provider-neutral multimodal content**
  ([ADR 0002](docs/adr/0002-multimodal-content.md)) — text/image content
  parts with the OpenAI mapping confined to the provider client.
- **Task dependencies + subagents**
  ([ADR 0003](docs/adr/0003-task-dependencies-and-subagents.md)) —
  `parentTaskId` (lineage) and `dependsOn` (claim gate, DAG by
  construction), lazy claim-time settling of doomed dependents, cooperative
  cancel cascade, `spawnChild`, opt-in priority aging, lease-gated progress.
- **`@agentkit/mcp-client`** ([ADR 0004](docs/adr/0004-mcp-client.md)) —
  MCP servers' tools as a `ToolSetContributor`: official
  `@modelcontextprotocol/sdk` for protocol, OneMind's battle-tested
  semantics on top (canonical ids, fail-closed collisions, circuit breaker,
  reconnect dedup), typed errors, `SecretStore`-resolved and redacted
  credentials. Host-level orphan tool-call reconciliation keeps provider
  history balanced across crashes.
- **`@agentkit/transport-http`** ([ADR 0005](docs/adr/0005-http-transport.md)) —
  the official optional adapter serving REST v1: fetch-standard handler,
  SSE replay-then-poll on a seq cursor with `Last-Event-ID` resume,
  required `Idempotency-Key` on submit, RFC 7807 errors.

## 0.1.0 — 2026-08 — Initial extraction

The original extraction from OpenPCB's assistant: `@agentkit/contracts`
(TypeBox schemas as the single source of shape truth), `@agentkit/core`
(pure `runChat` loop, OpenAI-compatible SSE client, Ajv tool registry),
`@agentkit/host` (ports, turn runner, staged-write proposals),
`@agentkit/testing` (mocks, golden traces, store conformance), and the
workspace-private reference adapters (memory + `bun:sqlite`).

## How this codebase is developed

The working method, for anyone continuing it:

- **Decisions are ADRs.** Anything that changes architecture or a
  contract lands with a numbered record in `docs/adr/` — problem, evidence,
  decision, alternatives, consequences. `docs/roadmap.md` is the sequenced
  backlog; finished phases move to its Done list.
- **Reference repos are read-only evidence.** Capabilities are extracted
  from the source projects (OpenPCB, OneMind) by porting their *proven
  semantics* — never by copying code wholesale. Each phase names the
  reference implementation it preserves, and each ADR records where the
  port deliberately deviates because the reference had a defect.
- **Verification is part of the definition of done.** Every landing passes
  typecheck, the full test suite, build, and lint before it is committed;
  each wave ends with an independent fresh-context adversarial review of
  the diff against the plan, and its findings are fixed (or explicitly
  recorded) before the wave is called finished. Tests are required to be
  mutation-killing — a test that survives the removal of the behavior it
  claims to pin is treated as a bug.
- **Invariants are tested as invariants.** The durable-execution and
  conversation-tree guarantees are checked by seeded, deterministic
  model-test drivers (`@agentkit/testing`), not only by example-based
  tests, and the store conformance suite runs identically against every
  adapter.
