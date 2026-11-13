# Architecture

This document describes the tree as it is today. Behaviour that is not
stated here is specified in the [ADRs](adr) and in [`docs/rust/`](rust).

The production backend runs entirely in Rust (`rust/crates/`, 24 crates,
[ADR 0021](adr/0021-layered-crate-architecture.md)). TypeScript is the API
surface around it:

- `@agentkit/native` is a thin Node-API binding over the Rust crates. It holds
  no backend logic.
- `@agentkit/core` is the public chat/agent-loop API. `runChat()` and
  `runAgentLoop()` execute on Rust only; the TypeScript loop engine is deleted.
  `AGENTKIT_BACKEND` may be unset or `native`; any other value (including the
  retired `ts`) fails closed with `invalid_input`.
- `@agentkit/contracts` holds wire DTOs and JSON Schemas. `@agentkit/client`
  and `@agentkit/react` consume the REST v1 + SSE contract.
- The former TypeScript backend was deleted in 0.8.0
  ([ADR 0023](adr/0023-retire-typescript-backend.md)). The Rust engine is the
  only backend.

Status of parity and qualification is tracked in
[`docs/rust/parity.md`](rust/parity.md) and
[`docs/rust/acceptance.md`](rust/acceptance.md); this file makes no
release-readiness claim.

## Layers and crates

Six layers; dependencies point inward only. The layer numbers below are the
`LAYERS` table in `rust/scripts/check-arch.mjs`, which enforces the ADR 0021
rules (R1–R13) over `cargo metadata`; violations live in
`rust/scripts/arch-allowlist.json` and may only shrink
(`bun run check:arch`).

| L | Crate | Responsibility |
|---|---|---|
| 1 | `agentkit-kernel` | SHA-256 hex, canonical JSON, and the single `Runtime` clock/identity port shared by every layer |
| 1 | `agentkit-contracts` | Wire DTOs, JSON Schema bundle with offline validation, REST route table, identity hashing |
| 2 | `agentkit-core` | Model/tool loop and its ports: provider streams, bounded tool execution, budgets |
| 2 | `agentkit-governance` | Ports only: tool guards, approvals, execution authority, receipts, proposals |
| 2 | `agentkit-native` | Versioned execution contracts, capability policy and injected ports for vendor agent runtimes ([ADR 0025](adr/0025-claude-code-native-backend-contracts.md)); also declares the native-lane ports `AttemptBrokerHandle` and `NativeRunRepository` (ADR 0024 addendum) |
| 3 | `agentkit-engine` | Generic durable task execution: `AgentHost`, worker, retry, subscription, the `TaskRepository` port and the task/lease/attempt model |
| 3 | `agentkit-sessions` | Durable agent-session service, agent/run profiles, run-content freezing |
| 3 | `agentkit-chat` | Chat execution and governance: `ChatExecutor`, compaction, correction passes, proposals, project instructions |
| 3 | `agentkit-agents` | Agent definitions, teammates, subagents and the advisor |
| 3 | `agentkit-native-exec` | Durable executor for vendor agent runtimes: `NativeAgentExecutor`, `NativeDispatchExecutor`, one owned process per attempt |
| 4 | `agentkit-store` | SQLite actor: tasks, attempts, leases, event log, conversations, sessions, native ledgers; the TypeScript-database importer lives inside it behind the `ts-import` feature ([ADR 0024](adr/0024-repository-ports.md)) |
| 4 | `agentkit-providers` | Concrete LLM provider adapters and provider HTTP plumbing |
| 4 | `agentkit-toolpacks-coding` | Confined coding tools: filesystem read/search/edit, read-only git, sandboxed commands |
| 4 | `agentkit-lsp` | LSP client, language-server lifecycle, code-intelligence tools |
| 4 | `agentkit-mcp` | MCP client and server transports on `rmcp` |
| 4 | `agentkit-connections` | App-scoped connections, consent, secret-store and network ports |
| 4 | `agentkit-native-broker` | One governed loopback MCP endpoint per live native attempt |
| 4 | `agentkit-claude` | Official Claude Code driver for native general Chat (ADR 0025) |
| 5 | `agentkit-runtime` | Composition: `AgentRuntime::builder()`, lane adapters, authority; the only crate that names concrete adapters |
| 6 | `agentkit-napi` | Node-API binding behind `@agentkit/native` |
| 6 | `agentkit-http` | Axum REST v1 + SSE over a `Backend` trait, and the `DesktopBackend` serving it |
| 6 | `agentkit-client` | Typed Rust HTTP and event-stream client |
| 6 | `agentkit-rust` (`crates/agentkit`) | Feature-selected SDK facade; contracts only by default; the `claude` feature carries the standalone one-turn native chat facade |

`agentkit-testkit` is dev-only: fakes, in-memory repositories and one
conformance suite per repository port (ADR 0024). The `LAYERS` table also
classifies reserved slots that do not exist under `rust/crates/` yet: three
Hybrid contexts at layer 3 and the platform-adapter slot at layer 4.

## Two execution owners

One admitted turn has exactly one execution owner
([ADR 0025](adr/0025-claude-code-native-backend-contracts.md) decision 1,
[ADR 0026](adr/0026-execution-owners-and-open-composition-root.md)): the host
claims the task under a lease fence and hands it to the executor registered
for its kind; that executor alone decides the terminal outcome.

- **The model loop.** `agentkit-core` runs the streaming model/tool loop;
  `agentkit-chat`'s `ChatExecutor` owns chat turns — provider passes, bounded
  tool execution, budgets, compaction, correction passes and proposals.
- **The vendor runtime lane.** `agentkit-native` declares the contracts and
  ports (`NativeBackend`, `ProcessSupervisor`, `ExecutableLocator`,
  `AttemptBrokerHandle`, `NativeRunRepository`); `agentkit-native-exec` runs
  the durable executor with one owned process per attempt;
  `agentkit-native-broker` serves the per-attempt governed tool endpoint;
  `agentkit-claude` is the driver. With a native lane configured the turn
  executor becomes `NativeDispatchExecutor`: native-target payloads run the
  durable native path, everything else keeps the model loop.

The driver stays outside the runtime: `agentkit-runtime` depends on
`agentkit-native` and `agentkit-native-exec` only, never on `agentkit-claude`
or `agentkit-native-broker`; the consumer injects the concrete backend,
supervisor and broker factory (ADR 0026).

## Composition root

`AgentRuntime::builder()` in `agentkit-runtime` (layer 5) is the composition
root: it registers executors per task kind (`executor(kind, executor)`,
`turn_kinds(kinds)`, `native_lane(lane)` — ADR 0026), wires the provider,
coding and authority lanes, and refuses a kind registered twice at build time.
It is the only crate that names concrete adapter types. The Node binding
builds its service through it; Rust embedders use the same builder.

## Runtime paths

```
In-process (Node/Bun)                 Out-of-process
  app / browser                         app / browser
   │ @agentkit/core, @agentkit/native     │ @agentkit/client (REST v1 + SSE)
   ▼                                     ▼
  agentkit-napi (cdylib)                agentkit-http (axum)
   │ AgentRuntime::builder()              │ Backend trait, implemented by
   ▼                                     │ DesktopBackend (holds AgentHost)
  agentkit-runtime ───────────────────────┤
                                          ▼
              contexts (engine ← sessions ← chat ← agents)
                     │ repository ports (ADR 0024)
                                          ▼
              agentkit-store (SQLite actor, leases, event log)
```

1. **In-process.** An application imports `@agentkit/core` and
   `@agentkit/native` (or the `agentkit` umbrella subpaths). The binding
   opens a `SqliteStore`, builds an `AgentRuntime`, and exposes handles,
   promises and pull-based durable-event subscriptions. Application callbacks
   (providers, tools, authorization, verification) cross back into JavaScript
   through the callback router; the R8 gate counts `#[napi]` exports that do
   not run under `catch_unwind` (allowlisted, removed by the NAPI phase). The
   binding can also start the HTTP server (`startHttpServer`) inside the same
   process.
2. **Out-of-process.** `agentkit-http` serves REST v1 and resumable SSE over
   an authenticated `Backend`; its `DesktopBackend` implements that trait from
   a local `AgentHost`, store, sessions and services. `@agentkit/client` (TypeScript)
   and `agentkit-client` (Rust) are the typed clients. The HTTP backend is a
   single-actor desktop boundary; multi-tenant use needs an isolated `Backend`
   supplied by the application (see [`rust/README.md`](../rust/README.md)).

## Who may name the store

Persistence is reached only through the repository ports each context owns
(ADR 0024): `agentkit_engine::TaskRepository` and the session, chat and agent
ports, implemented by the SQLite actor in `agentkit-store`. Adapters depend
on the context crates, never the reverse. Remaining named exceptions, each
allowlisted with the phase that removes it:

- the contexts `agentkit-sessions`, `agentkit-chat` and `agentkit-agents`
  still depend on `agentkit-store` directly (R1b entries, removed by P5e);
- among driven adapters only `agentkit-napi` may depend on the store (R11);
  `agentkit-http` and `agentkit-mcp` are allowlisted at intake.

With the allowlists empty, `agentkit-runtime` and `agentkit-napi` are the only
crates naming the concrete store.

## Durable execution model

Execution is a durable queue in `agentkit-store`, driven by `AgentHost`
(`rust/crates/engine/src/runner.rs`). Terminology follows the Rust types
(`TaskStatus`, `AttemptRecord`, `Lease`, `LeaseFence` in
`rust/crates/engine/src/model.rs`).

**Tasks and attempts.** A task has a `kind` and a `scope_id`; kind-dispatched
executors run it (`ChatExecutor` is the chat executor; the native executors
cover native-target payloads). Each execution is an *attempt*. Attempt end
statuses are `completed`, `failed`, `abandoned` and `cancelled`; `abandoned`
is the crash outcome: the lease expired while the attempt was running, so
recovery ends it that way rather than guessing success or failure. Attempts
end with their first terminal status; a second call does not overwrite it.

**Task statuses.** `Queued`, `Running`, `Interrupted`, `WaitingApproval`,
`WaitingChild`, `Uncertain` (non-terminal), and `Completed`, `Failed`,
`Cancelled` (terminal; `TaskStatus::is_terminal`). `WaitingApproval` and
`WaitingChild` are durable parks that hold no execution slot.
`Uncertain` is an attempt ended without a decidable outcome; only an explicit
`resolve_uncertain` settles it, so a late worker cannot restate finished work.

**Leases and fencing.** `Lease { task_id, attempt_id, owner_id, lease_token,
fencing_token, expires_at }`. `lease_token` proves current ownership on every
write. `fencing_token` comes from one store-wide counter, so it is monotonic
across all leases ever issued: a worker that paused and woke believing it
still owns the task is rejected by comparison. Fenced engine writes (event
append, progress, conversation and session mutations, terminal task and
attempt writes) carry a `LeaseFence` and are verified inside the same
transaction as the write; a stale fence yields a lease-lost repository fault.
Expiry alone is sufficient to reject a worker, before any replacement owner
acquires the lease.

**Recovery.** Expired leases are collected and recovery ends their attempts
`abandoned`, increments the task's poison count and applies the configured
`RecoveryMode` on `HostOptions` (engine):

- cancel requested and the task is agent-bound: `Uncertain`
  (`cancel_requested_outcome_unknown`); cancel requested otherwise:
  `Cancelled`;
- `Manual` (the `HostOptions` default): `Interrupted`, requiring an explicit
  `resume()`;
- `Automatic`: back to `Queued` after the retry delay with a new attempt, or
  dead-lettered (`failed`, `attempts_exhausted`) once `attempt_count` reaches
  `max_attempts`.

Call `recover()` before `start_worker()`; await the worker's `shutdown()`
before releasing the database owner.

**Event log.** Every run writes to a per-task durable event log with a
monotonic sequence; the log is the source for rebuilding projections, and
replay never re-executes a domain operation. Event persistence is awaited: a
failed persistence prevents the next tool or provider operation.

**One live turn per chat.** Chat submission creates its task with
`exclusive_scope`; the store refuses a second unfinished task in the scope in
the same transaction (`ScopeBusy`), surfaced as `chat_busy` (HTTP 409). A
redelivered task id is answered as a duplicate, not refused.

**At-least-once delivery, idempotent effects.** No exactly-once claim is made.
Task submission is idempotent per caller-supplied task id. `ProposalService`
(`agentkit-chat`) validates proposal identity and current authorization
before returning a receipt, and reconciles ambiguous application outcomes
without automatically reapplying them: "cannot prove it landed" is treated as
failure. Completed tool effects need domain receipts; cancellation, timeouts
and transport exceptions do not imply an external mutation was rolled back.

**A run is not one pass.** One task can drive several passes (chat-only retry,
empty-response retry, correction rounds, [ADR
0012](adr/0012-multi-pass-correction-harness.md)), each with its own
`run.started` … terminal pair on the same log. Every pass after the first is
announced by `run.warning { code: "retry_pass", pass, reason }`; consumers
drop text streamed so far at that boundary. Hence: the SSE stream ends only on
the task's settlement (`agentkit.stream.settled`, sent after the task is
terminal or interrupted and the final event drain), not on the first terminal
run event; clients report provider outcomes as settling until then.

**Subscriptions never cancel.** `RunHandle::subscribe()` (engine) follows
committed events over a bounded channel. Closing a subscription or an SSE
connection does not cancel execution; cancel is explicit
(`RunHandle::cancel()`, the cancel route). An SSE resume with an unknown
`Last-Event-ID` announces a full replay (`x-agentkit-stream-resync` header and
an opening `agentkit.stream.resync` frame).

Further detail: [`docs/execution-lifecycle.md`](execution-lifecycle.md),
[ADR 0016](adr/0016-durable-agent-sessions.md),
[ADR 0020](adr/0020-execution-ownership-and-recovery.md),
[`docs/rust/native-admission.md`](rust/native-admission.md).

## TypeScript packages

| Package | Role |
|---|---|
| `@agentkit/native` | Live. Node-API binding wrappers (`src/`), committed napi loader and `index.d.ts` (`generated/`), per-platform addons `@agentkit/native-<platform>` |
| `@agentkit/core` | Live. Public loop API over Rust (`runChat`, `runAgentLoop`), tool registry and content-part helpers |
| `@agentkit/contracts` | Live. Wire DTOs, JSON Schemas (TypeBox), REST v1 route table; hand-written mirror of the Rust contracts (below) |
| `@agentkit/client` | Live. Typed REST v1 + SSE client, auto-resuming `streamRun`, derived run phases |
| `@agentkit/react` | Live. Headless hooks over `@agentkit/client` |
| `@agentkit/testing` | Live, dev. Mocks, fixtures, golden traces, conformance suites |
| `agentkit` (`packages/agentkit`) | Live. Umbrella package with subpath exports |

The six frozen TypeScript backend packages (`host`, `adapters-memory`,
`adapters-sqlite`, `runner-local`, `transport-http`, `mcp-client`) were deleted
in 0.8.0 ([ADR 0023](adr/0023-retire-typescript-backend.md)); the umbrella
dropped their subpaths. Their source is recoverable from commit `a31dcb1`
(`git show a31dcb1:packages/host/src/...`). The package ledger is in
[`docs/rust/parity.md`](rust/parity.md).

## Contracts

`agentkit-contracts` is the wire authority ([ADR 0022](adr/0022-rust-contract-authority.md)):
hand-owned DTOs in `rust/crates/contracts/src/dto/`, the route table in
`routes.rs` and the JSON Schema bundle `schema_bundle.json`, which the crate
validates against offline. [`packages/contracts`](../packages/contracts/src)
(TypeBox schemas and `src/rest.ts`) is the hand-written TypeScript mirror with a
frozen public API; `bun run contracts:check` asserts that it emits exactly the
Rust bundle. There is no code generator. Contract `0.7.0`, REST `v1`. See
[`docs/contracts.md`](contracts.md) for the event vocabulary, warning codes and
tool envelope.

## Quality gates

There is no hosted CI; all gates run locally, from the repo root with bun and
from `rust/` with cargo. See
[`DEVELOPING.md`](../DEVELOPING.md#local-gates-no-hosted-ci) for
`bun run qualify:local` and the individual checks.

- `bun run check:arch` — ADR 0021 rules over `cargo metadata`
  (`rust/scripts/check-arch.mjs`, allowlist
  `rust/scripts/arch-allowlist.json`).
- `bun run check:deny` / `bun run check:machete` — cargo-deny and unused
  dependencies.
- `bun run contracts:check` — the TypeScript contract mirror equals the Rust
  bundle; `contracts:generate` refreshes the exported fixtures.
- `bun run check:rust-api` / `check:api` — public API snapshots
  (`rust/scripts/public-api.mjs`, `scripts/check-public-api.mjs`).
- `bun run check:native-package` / `check:native-umbrella` — binding and
  umbrella packaging (`rust/scripts/`).
- `bun run lint` (biome), `bun run typecheck`, `bun test`,
  `bun run test:coverage`.
- `bun run smoke:node` / `smoke:pack` / `smoke:umbrella`,
  `bun run qualify:local` / `qualify:cold` — packaging and cold-install
  verification.

## Further reading

- ADRs in [`docs/adr/`](adr): runtime and execution
  [0015](adr/0015-unified-agent-runtime.md),
  [0016](adr/0016-durable-agent-sessions.md),
  [0017](adr/0017-context-assembly-and-compaction.md),
  [0018](adr/0018-coding-tool-pack.md),
  [0019](adr/0019-human-approval-plan-mode-and-side-effect-receipts.md),
  [0020](adr/0020-execution-ownership-and-recovery.md),
  [0021](adr/0021-layered-crate-architecture.md),
  [0024](adr/0024-repository-ports.md),
  [0025](adr/0025-claude-code-native-backend-contracts.md),
  [0026](adr/0026-execution-owners-and-open-composition-root.md),
  [0023](adr/0023-retire-typescript-backend.md). Earlier ADRs (0001–0014) are
  history; where they describe TypeScript implementations they are superseded
  by the Rust engine.
- [`docs/rust/`](rust): `parity.md`, `acceptance.md`, `native-admission.md`,
  `native-binding-milestone.md`, `provider-authority.md`,
  `numeric-event-cursors.md`.
- [`docs/contracts.md`](contracts.md), [`docs/ports.md`](ports.md) (port catalog;
  TypeScript reference implementations deleted in 0.8.0), [`docs/non-goals.md`](non-goals.md),
  [`docs/roadmap.md`](roadmap.md).
- [`rust/README.md`](../rust/README.md): embedding, connections and consent,
  verification commands.
