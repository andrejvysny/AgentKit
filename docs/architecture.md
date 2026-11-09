# Architecture

This document describes what runs today, then the target layering accepted in
[ADR 0021](adr/0021-layered-crate-architecture.md). Behaviour that is not
stated here is specified in the ADRs and in `docs/rust/`.

## Overview

The production backend runs entirely in Rust (`rust/crates/*`): durable task
store, execution host, chat and agent sessions, governance, provider
transports, MCP client and server, REST/SSE server, coding tool pack.
TypeScript is the API surface around it:

- `@agentkit/native` is a thin Node-API binding over the Rust crates. It holds
  no backend logic.
- `@agentkit/core` is the public chat/agent-loop API. `runChat()` and
  `runAgentLoop()` execute on Rust only; the TypeScript loop engine is deleted.
  `AGENTKIT_BACKEND` may be unset or `native`; any other value (including the
  retired `ts`) fails closed with `invalid_input`.
- `@agentkit/contracts` holds wire DTOs and JSON Schemas. `@agentkit/client`
  and `@agentkit/react` consume the REST v1 + SSE contract.
- `@agentkit/host`, `@agentkit/adapters-memory`, `@agentkit/adapters-sqlite`,
  `@agentkit/runner-local`, `@agentkit/transport-http` and
  `@agentkit/mcp-client` are frozen libraries slated for deletion (see
  [TypeScript packages](#typescript-packages)). They are not on any
  production path.

Status of parity and qualification is tracked in
[`docs/rust/parity.md`](rust/parity.md) and
[`docs/rust/acceptance.md`](rust/acceptance.md); this file makes no
release-readiness claim.

## Runtime paths

```
In-process (Node/Bun)                 Out-of-process
  app                                   app / browser
   │ @agentkit/core, @agentkit/native     │ @agentkit/client (REST v1 + SSE)
   ▼                                      ▼
  agentkit-napi (cdylib)                agentkit-transport-http (axum)
   │                                      │ Backend trait
   ▼                                      ▼
  agentkit-host ── agentkit-core ◀── agentkit-host (http feature)
   │
   ▼
  agentkit-store (SQLite actor, leases, event log)
```

1. **In-process.** An application imports `@agentkit/core` and
   `@agentkit/native` (or the `agentkit` umbrella subpaths). The binding
   (`agentkit-napi`) opens a `SqliteStore`, builds an `AgentHost`, and exposes
   handles, promises and pull-based durable-event subscriptions. Application
   callbacks (providers, tools, authorization, verification) cross back into
   JavaScript through the callback router; every `#[napi]` export is declared
   with `catch_unwind`. The binding can also start the HTTP server
   (`startHttpServer`) inside the same process.
2. **Out-of-process.** `agentkit-transport-http` serves REST v1 and resumable
   SSE over an authenticated `Backend`; `agentkit-host` implements that
   `Backend` behind its `http` feature. `@agentkit/client` (TypeScript) and
   `agentkit-client` (Rust) are the typed clients. The HTTP backend is a
   single-actor desktop boundary; multi-tenant use needs an isolated `Backend`
   supplied by the application (see [`rust/README.md`](../rust/README.md)).

Both paths run the same Rust host over the same store. Rust embedders (the
egui reference app) use the host directly.

## Rust crates today

All crates live in `rust/crates/`. "Depends on" lists workspace crates only
(runtime dependencies, from the manifests).

| Crate | Responsibility | Depends on |
|---|---|---|
| `agentkit-kernel` | SHA-256 hex and canonical JSON shared by contracts, store and host | none |
| `agentkit-contracts` | Serde contracts, offline schema validation, REST route table, idempotency; generated DTOs | kernel |
| `agentkit-core` | Provider protocols, streaming, bounded tool execution, budgets | contracts |
| `agentkit-store` | SQLite actor: tasks, attempts, leases, event log, conversations, projections, recovery | contracts, kernel |
| `agentkit-connections` | App-scoped connections, consent, secret-store and network ports | contracts |
| `agentkit-host` | Durable execution (`AgentHost`, worker), chat (`ChatExecutor`), agent sessions (`AgentSessionService`), profiles, governance, subagents, advisor, teammates, proposals, MCP configuration, optional HTTP backend | store, core, contracts, kernel, connections, transport-http (optional) |
| `agentkit-transport-http` | Axum REST/SSE boundary over a `Backend` trait | contracts |
| `agentkit-client` | Native HTTP client, replay decoding, presentation state | contracts |
| `agentkit-mcp` | MCP client and server transports on `rmcp` | contracts, core |
| `agentkit-toolpacks-coding` | Confined coding tools: filesystem, read-only git, sandboxed commands | contracts, core, host |
| `agentkit-lsp` | LSP client, language-server lifecycle, code-intelligence tools | contracts, core |
| `agentkit-napi` | Node-API binding behind `@agentkit/native` | store, host (http), core, connections, mcp, contracts, transport-http, toolpacks-coding, lsp |
| `agentkit-rust` (`crates/agentkit`) | Feature-selected integration facade; contracts only by default | contracts; others optional |

`agentkit-host` is the large crate: ADR 0021 exists because it owns most
application logic and reaches modules through its root. Run
`bun run check:arch` for the current dependency-rule violations (the
allowlist is `rust/scripts/arch-allowlist.json`).

## Durable execution model

Execution is a durable queue in `agentkit-store`, driven by `AgentHost`.
Terminology follows the Rust types (`TaskStatus`, `AttemptRecord`, `Lease`,
`LeaseFence` in `rust/crates/store/src/models.rs`).

**Tasks and attempts.** A task has a `kind` and a `scope_id`; kind-dispatched
executors run it (`ChatExecutor` is the chat executor). Each execution is an
*attempt*. Attempt end statuses are `completed`, `failed`, `abandoned` and
`cancelled`; `abandoned` is the crash outcome: the lease expired while the
attempt was running, so recovery ends it that way rather than guessing success
or failure. Attempts end with their first terminal status; a second call does
not overwrite it.

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
transaction as the write; a stale fence yields `StoreError::LeaseLost`.
Expiry alone is sufficient to reject a worker, before any replacement owner
acquires the lease.

**Recovery.** `expire_stale_leases` collects expired leases and recovery ends
their attempts `abandoned`, increments the task's poison count and applies the
configured `RecoveryMode`:

- cancel requested and the task is agent-bound: `Uncertain`
  (`cancel_requested_outcome_unknown`); cancel requested otherwise: `Cancelled`;
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
the same transaction (`StoreError::ScopeBusy`), surfaced as `chat_busy` (HTTP 409). A redelivered task
id is answered as a duplicate, not refused.

**At-least-once delivery, idempotent effects.** No exactly-once claim is made.
Task submission is idempotent per caller-supplied task id. `ProposalService`
validates proposal identity and current authorization before returning a
receipt, and reconciles ambiguous application outcomes without automatically
reapplying them: "cannot prove it landed" is treated as failure. Completed
tool effects need domain receipts; cancellation, timeouts and transport
exceptions do not imply an external mutation was rolled back.

**A run is not one pass.** One task can drive several passes (chat-only retry,
empty-response retry, correction rounds, [ADR
0012](adr/0012-multi-pass-correction-harness.md)), each with its own
`run.started` … terminal pair on the same log. Every pass after the first is
announced by `run.warning { code: "retry_pass", pass, reason }`; consumers
drop text streamed so far at that boundary. Hence: the SSE stream ends only on
the task's settlement (`agentkit.stream.settled`, sent after the task is
terminal or interrupted and the final event drain), not on the first terminal
run event; clients report provider outcomes as settling until then.

**Subscriptions never cancel.** `RunHandle::subscribe()` follows committed
events over a bounded channel. Closing a subscription or an SSE connection does
not cancel execution; cancel is explicit (`RunHandle::cancel()`, the cancel
route). An SSE resume with an unknown `Last-Event-ID` announces a full replay
(`x-agentkit-stream-resync` header and an opening `agentkit.stream.resync`
frame).

Further detail: [`docs/execution-lifecycle.md`](execution-lifecycle.md),
[ADR 0016](adr/0016-durable-agent-sessions.md),
[ADR 0020](adr/0020-execution-ownership-and-recovery.md),
[`docs/rust/native-admission.md`](rust/native-admission.md).

## TypeScript packages

| Package | Role |
|---|---|
| `@agentkit/native` | Live. Node-API binding wrappers (`src/`), committed napi loader and `index.d.ts` (`generated/`), per-platform addons `@agentkit/native-<platform>` |
| `@agentkit/core` | Live. Public loop API over Rust (`runChat`, `runAgentLoop`), tool registry and content-part helpers |
| `@agentkit/contracts` | Live. Wire DTOs, JSON Schemas (TypeBox), REST v1 route table; codegen input (below) |
| `@agentkit/client` | Live. Typed REST v1 + SSE client, auto-resuming `streamRun`, derived run phases |
| `@agentkit/react` | Live. Headless hooks over `@agentkit/client` |
| `@agentkit/testing` | Live, dev. Mocks, fixtures, golden traces, conformance suites |
| `agentkit` (`packages/agentkit`) | Live. Umbrella package with subpath exports |
| `@agentkit/host` | Frozen. `TurnRunner`, `TaskService`, `ConversationService` remain exported; only static `REASONING_EFFORTS` metadata is imported in production |
| `@agentkit/adapters-memory`, `@agentkit/adapters-sqlite` | Frozen. Production storage is Rust `SqliteStore` |
| `@agentkit/runner-local` | Frozen. Queue, worker and lease engine live in Rust |
| `@agentkit/transport-http` | Frozen. Production REST/SSE is the Rust server; the TS server remains the behavioural reference the Rust route table is proven against |
| `@agentkit/mcp-client` | Frozen. Production MCP is the Rust client and server |

Frozen packages persist under the API-freeze constraint and are exercised by
their own suites. They are slated for deletion in the ADR 0021 migration (phase
P6, tracked in Plane). No live package, example or script imports them (the
decoupling step removed the composition-ownership gate along with the last
imports). The package ledger is in
[`docs/rust/parity.md`](rust/parity.md).

## Contracts and codegen

[`packages/contracts`](../packages/contracts/src) (TypeBox schemas and
`src/rest.ts`) is the authoritative wire source today.
`rust/crates/contracts/codegen/generate.mjs` (run with Bun from the repository
root) reads it and writes the Rust DTOs under `rust/crates/contracts/src/generated/`
and `schema_bundle.json`. The Rust crate validates offline against that
bundle. The wire authority is contract `0.7.0`, REST `v1`.

The Rust contract definitions will become authoritative and generate the
TypeScript schemas (decided 2026-11-09; recorded in ADR 0022 when the flip
lands, after the TypeScript backend is retired). Until then TypeBox stays the
source of truth. See
[`docs/contracts.md`](contracts.md) for the event vocabulary, warning codes and
tool envelope.

## Target architecture

[ADR 0021](adr/0021-layered-crate-architecture.md) (accepted 2026-11-09)
splits the workspace into six layers; dependencies point inward only:

1. Foundation: `agentkit-kernel`, `agentkit-contracts`.
2. Agent loop and governance: `agentkit-core`, `agentkit-governance`.
3. Application contexts, each owning its repository traits:
   `agentkit-engine` ← `agentkit-sessions` ← `agentkit-chat` ←
   `agentkit-agents`; future Hybrid contexts depend on engine and sessions only.
4. Driven adapters: `agentkit-store-sqlite`, `agentkit-store-legacy-import`,
   `agentkit-providers`, `agentkit-toolpacks-coding`, `agentkit-lsp`,
   `agentkit-mcp`, `agentkit-connections`, `agentkit-sys`.
5. Composition: `agentkit-runtime` (`AgentRuntime::builder()`), the only crate
   naming concrete adapters.
6. Driving adapters: `agentkit-napi`, `agentkit-http`, `agentkit-client`, the
   `agentkit` facade.

`agentkit-testkit` (dev-only) provides fakes, in-memory repositories and
repository conformance suites. `agentkit-host` is removed once its modules have
moved, with no re-export shim. Rules R1–R10 (no `axum`/`napi`/`rusqlite` in
layers 1–3, leftward-only context dependencies, no `pub use x::*`, enum error
types, `catch_unwind` on napi exports, and so on) are enforced by
`rust/scripts/check-arch.mjs` (`bun run check:arch`), cargo-deny
(`rust/deny.toml`) and review. Only `agentkit-kernel` exists from this layout
so far; the rest is migration work (phases P0–P8, tracked in Plane).

## Quality gates

There is no hosted CI; all gates run locally. See
[`DEVELOPING.md`](../DEVELOPING.md#local-gates-no-hosted-ci) for
`bun run qualify:local` and the individual checks.

## Further reading

- ADRs in [`docs/adr/`](adr): runtime and execution
  [0015](adr/0015-unified-agent-runtime.md),
  [0016](adr/0016-durable-agent-sessions.md),
  [0017](adr/0017-context-assembly-and-compaction.md),
  [0018](adr/0018-coding-tool-pack.md),
  [0019](adr/0019-human-approval-plan-mode-and-side-effect-receipts.md),
  [0020](adr/0020-execution-ownership-and-recovery.md),
  [0021](adr/0021-layered-crate-architecture.md). Earlier ADRs (0001–0014) are
  history; where they describe TypeScript implementations they are superseded
  by the Rust engine.
- [`docs/rust/`](rust): `parity.md`, `acceptance.md`, `native-admission.md`,
  `native-binding-milestone.md`, `provider-authority.md`,
  `numeric-event-cursors.md`.
- [`docs/contracts.md`](contracts.md), [`docs/ports.md`](ports.md) (TypeScript
  port catalog, frozen), [`docs/non-goals.md`](non-goals.md),
  [`docs/roadmap.md`](roadmap.md).
- [`rust/README.md`](../rust/README.md): embedding, connections and consent,
  verification commands.
