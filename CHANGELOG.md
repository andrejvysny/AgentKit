# Changelog

Versions track `CONTRACT_VERSION` (the event/DTO shape version in
`@agentkit/contracts`), not npm releases — nothing is published yet. Each
entry links the architecture decision records (`docs/adr/`) that carry the
full reasoning; this file is the short answer to "what changed and why",
newest first.

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
