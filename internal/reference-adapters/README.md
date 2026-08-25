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
  (`transitionTask`, `ProposalStore.transition`) use a guarded
  `UPDATE ... WHERE id = ? AND status = ?` and check the driver's reported
  `changes` count as the backstop against a race the initial `SELECT`
  could not see.
- **`SingleProcessTaskRunner`** (`src/task-runner/single-process-task-runner.ts`)
  — a complete `TaskRunner` for one process: claim, execute, heartbeat,
  retry (classified — see `error-classifier.ts`, unknown failures are
  terminal, not retried forever), dead-letter, recover. Dispatch is
  fire-and-forget: the claim loop never awaits an execution, so
  `concurrency: N` actually runs N attempts at once. Retry happens **in
  place** — a task that started stays `running` for its whole life and gets
  a new attempt (new lease, new fencing token) rather than going back to
  `queued`, because the `TaskStore` transition table has no
  `running → queued` edge.
- **`ScopeLock`** (`src/task-runner/scope-lock.ts`) — in-memory, per-process
  serialization so two tasks sharing a scope (usually a chat id) do not
  execute concurrently. A dispatch optimization only: correctness rests on
  the store's `claimNext` + leases, not on this lock — wiping it between two
  claims would not let two workers claim the same task.
- **`error-classifier.ts`** — `classifyExecutionError`: transient / terminal
  / cancelled, from structured signals (a host error `code`, an explicit
  `retryable` flag, an HTTP status) before falling back to message
  heuristics. Unrecognized failures are terminal by default.
- **`task-aging.ts`** — the priority-aging formula both stores' `claimNext`
  use, preserved from task-system but **opt-in**: `agingBonus` defaults to
  `0` (aging off; ordering is plain `priority DESC, enqueuedAt ASC`),
  `agingIntervalMs` defaults to 30s, and `agingMaxBonus` defaults to
  uncapped once a host does opt in. Passed as
  `MemoryAssistantStoreOptions`/`SqliteAssistantStoreOptions` at
  construction (both extend `TaskAgingOptions`), not a runtime toggle. See
  [ADR 0003](../../docs/adr/0003-task-dependencies-and-subagents.md).

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

## Transaction isolation caveat

Neither store isolates a transaction from concurrent work on the **same store
instance**. `transaction()` gives atomicity (all-or-nothing on the sqlite
store), not isolation: over `bun:sqlite`, concurrent store calls issued while a
transaction callback awaits genuinely-async work JOIN that transaction and roll
back with it — the connection has exactly one open transaction, and every
statement issued on it belongs to that one. Keep transaction callbacks free of
foreign async work: await the model, the applier, or another subsystem
*outside*, then pass the results in.

## Single-process limits

Documented in `single-process-task-runner.ts`'s module doc, and worth
repeating here: cancellation of a task is delivered by aborting an in-memory
`AbortController` this process registered. A cancel aimed at a task some
*other* process is executing does nothing — a durable, cross-process cancel
needs a cancellation flag in the store that every worker polls, which is a
different design with a different cost than this reference adapter takes
on. See [`docs/non-goals.md`](../../docs/non-goals.md).

## SQLite schema (v4)

Single-file DDL in `src/sqlite/schema.ts` (`SCHEMA_V4`), applied
idempotently (`CREATE ... IF NOT EXISTS`, `INSERT OR IGNORE`). No
migrations ship in this workspace-private adapter — `PRAGMA user_version`
guards against opening a database written by a different schema version; a
stale dev database (including one written by v3) is recreated, not upgraded
in place.

v4 added conversation branching: `messages` gained `parent_message_id` (a
self-FK), `depth`, `branch_index` and `active`, plus the two indexes those
reads want (`(chat_id, active, depth)` for the active path, `(parent_message_id,
branch_index)` for siblings). A chat is a tree; `active` is the per-message flag
marking which root-to-leaf path through it the conversation currently is.

Table-to-port mapping:

| Table(s) | Port |
|---|---|
| `chats`, `messages` | `ConversationStore` |
| `tasks`, `task_attempts`, `leases`, `task_events` | `TaskStore` |
| `proposals`, `proposal_outcomes` | `ProposalStore` |
| `providers`, `provider_models`, `provider_capabilities` | `ProviderStore` |
| `settings` (single row, `id = 1`) | `SettingsStore` |
| `outbox` | `OutboxStore` |
| `fencing_counter` (single row) | *not a port record* — backs `TaskStore.acquireLease`'s store-global monotonic fencing token. |

Notes:

- Queue state lives on `tasks` + `leases`; there is no separate queue table
  — `claimNext` computes effective priority (base priority plus an aging
  term, off by default — see `task-aging.ts` below) in the query itself, so
  nothing can drift out of sync. `tasks` has no
  `chat_id` column — a task of an arbitrary kind has no conversation, so
  whatever a kind needs (a chat turn's `chatId` included) rides in the
  `payload` column instead. `parent_task_id` (lineage) and `depends_on` (a
  JSON array of task ids, the claim gate) are separate columns, neither a
  foreign key — `createTask` proves both point at existing rows before it
  writes, and an FK would additionally block deleting an old completed task
  a finished row still names. `progress` is an overwritten JSON snapshot,
  never appended to.
- The idempotency guarantee for model-issued writes is a partial unique
  index: `UNIQUE(scope_key, action_id) WHERE action_id IS NOT NULL AND
  status NOT IN ('rejected', 'invalidated')` — see
  [`docs/ports.md`](../../docs/ports.md#proposalstore).
- JSON-shaped fields (`payload`, `metadata`, `envelope`, `operations`,
  `warnings`, `tool_calls`, `failed_ops`, `extra_headers`,
  `decision`, ...) are stored as `TEXT`; the store (de)serializes them,
  SQLite never inspects their contents.

## License

MIT
