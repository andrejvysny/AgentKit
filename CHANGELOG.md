# Changelog

Versions track `CONTRACT_VERSION` (the event/DTO shape version in
`@agentkit/contracts`), not npm releases — nothing is published yet. Each
entry links the architecture decision records (`docs/adr/`) that carry the
full reasoning; this file is the short answer to "what changed and why",
newest first.

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
