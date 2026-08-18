# @agentkit/reference-adapters

**Reference-grade, single-process implementations of every `@agentkit/host`
port, for local development and tests — not published, not a template for a
production deployment.**

This is a workspace-private package (`"private": true`; workspace path
`internal/reference-adapters`, not `packages/`). It exists so this monorepo
can exercise `@agentkit/host` end-to-end without every consumer writing its
own `AssistantStore`/`TaskRunner` first, and so a new adapter author has a
complete, working example to read. It is explicitly **not** where a
distributed or multi-tenant deployment's storage layer belongs — a
cloud-agent service needs adapters over a networked backend (Postgres,
Redis, or similar) implementing the same ports; see
[`docs/non-goals.md`](../../docs/non-goals.md).

## What's here

- **`MemoryAssistantStore`** (`src/memory/memory-assistant-store.ts`) —
  every port backed by JS `Map`s. `transaction(fn)` runs `fn(this)`
  directly — there is **no rollback**; a throw after some writes leaves
  those writes in place. Good for tests and a single-process embed that
  does not need crash-consistency; report
  `capabilities: { atomicTransactions: false }` to the conformance suite
  when building something similar.
- **`SqliteAssistantStore`** (`src/sqlite/sqlite-assistant-store.ts`) — every
  port backed by `bun:sqlite`, with real `BEGIN`/`COMMIT`/`ROLLBACK`
  transactions. Fencing/CAS pattern: a lease-guarded write reads the
  current lease inside the same transaction, then performs its
  `INSERT`/`UPDATE`, and rejects on mismatch; compare-and-set operations
  (`transitionRun`, `ProposalStore.transition`) use a guarded
  `UPDATE ... WHERE id = ? AND status = ?` and check the driver's reported
  `changes` count as the backstop against a race the initial `SELECT`
  could not see.
- **`SingleProcessTaskRunner`** (`src/task-runner/single-process-task-runner.ts`)
  — a complete `TaskRunner` for one process: claim, execute, heartbeat,
  retry (classified — see `error-classifier.ts`, unknown failures are
  terminal, not retried forever), dead-letter, recover. Dispatch is
  fire-and-forget: the claim loop never awaits an execution, so
  `concurrency: N` actually runs N attempts at once. Retry happens **in
  place** — a run that started stays `running` for its whole life and gets
  a new attempt (new lease, new fencing token) rather than going back to
  `queued`, because the `RunStore` transition table has no
  `running → queued` edge.
- **`ScopeLock`** (`src/task-runner/scope-lock.ts`) — in-memory, per-process
  serialization so two runs sharing a scope (usually a chat id) do not
  execute concurrently. A dispatch optimization only: correctness rests on
  the store's `claimNext` + leases, not on this lock — wiping it between two
  claims would not let two workers claim the same run.
- **`error-classifier.ts`** — `classifyExecutionError`: transient / terminal
  / cancelled, from structured signals (a host error `code`, an explicit
  `retryable` flag, an HTTP status) before falling back to message
  heuristics. Unrecognized failures are terminal by default.

Both stores pass `@agentkit/testing`'s
`describeAssistantStoreConformance` suite — see
`tests/memory-conformance.test.ts` and `tests/sqlite-conformance.test.ts` —
plus adapter-specific tests (`tests/sqlite-specific.test.ts`,
`tests/scope-lock.test.ts`, `tests/error-classifier.test.ts`,
`tests/task-runner.test.ts`) and the full-stack smoke test
`tests/task-runner-integration.test.ts` (real `TurnRunner` +
`SingleProcessTaskRunner` + `MemoryAssistantStore`, only the provider
mocked — see
[`packages/host/README.md`](../../packages/host/README.md#embedding-turnrunner)).

## Single-process limits

Documented in `single-process-task-runner.ts`'s module doc, and worth
repeating here: cancellation of a run is delivered by aborting an in-memory
`AbortController` this process registered. A cancel aimed at a run some
*other* process is executing does nothing — a durable, cross-process cancel
needs a cancellation flag in the store that every worker polls, which is a
different design with a different cost than this reference adapter takes
on. See [`docs/non-goals.md`](../../docs/non-goals.md).

## SQLite schema (v1)

Single-file DDL in `src/sqlite/schema.ts` (`SCHEMA_V1`), applied
idempotently (`CREATE ... IF NOT EXISTS`, `INSERT OR IGNORE`). Table-to-port
mapping:

| Table(s) | Port |
|---|---|
| `chats`, `messages` | `ConversationStore` |
| `runs`, `run_attempts`, `leases`, `run_events` | `RunStore` |
| `proposals`, `proposal_outcomes` | `ProposalStore` |
| `providers`, `provider_models`, `provider_capabilities` | `ProviderStore` |
| `settings` (single row, `id = 1`) | `SettingsStore` |
| `outbox` | `OutboxStore` |
| `fencing_counter` (single row) | *not a port record* — backs `RunStore.acquireLease`'s store-global monotonic fencing token. |

Notes:

- Queue state lives on `runs` + `leases`; there is no separate queue table
  — `claimNext` computes effective priority (base priority + an age bucket)
  in the query itself, so nothing can drift out of sync.
- The idempotency guarantee for model-issued writes is a partial unique
  index: `UNIQUE(scope_key, action_id) WHERE action_id IS NOT NULL AND
  status NOT IN ('rejected', 'invalidated')` — see
  [`docs/ports.md`](../../docs/ports.md#proposalstore).
- JSON-shaped fields (`request`, `metadata`, `envelope`, `operations`,
  `warnings`, `tool_calls`, `payload`, `failed_ops`, `extra_headers`,
  `decision`, ...) are stored as `TEXT`; the store (de)serializes them,
  SQLite never inspects their contents.

## License

MIT
