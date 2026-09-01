# @agentkit/adapters-sqlite

**The durable `AssistantStore` for a single-process host: every
`@agentkit/host` port over `bun:sqlite`, with real transactions, lease
fencing, and compare-and-set transitions.**

This is the **production** store for a host that runs in one process — a
desktop app, a CLI, a sidecar. It is not a demo of what a store could look
like: it holds the same invariants under a fault-injection suite, a seeded
concurrency schedule, and a two-process contention test that a networked
adapter would have to hold.

```ts
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";

const store = new SqliteAssistantStore("./data/agentkit.sqlite");
// ... use it as an AssistantStore; call store.close() on shutdown.
```

## Bun only

`bun:sqlite` is a Bun built-in, so this package **does not load under plain
Node** — that is why its `engines` names `bun` and no `node`, and why it is
the one published `@agentkit/*` package excluded from the repo's
Node-loadability checks (`scripts/node-smoke.mjs` and CI's "shippable dists
import nothing from bun" grep; it is still packed and installed by
`scripts/pack-smoke.mjs`). Everything else in the workspace stays plain,
portable JavaScript. A host that must run on Node wants a different adapter
over the same ports; the port surface is unchanged, so only the construction
site moves.

## It owns its database file

**Give it a path nothing else manages.** The store applies its own schema on
open and guards the file with `PRAGMA user_version`: a database written by a
different schema version is refused, and a stale *dev* database is recreated
rather than upgraded in place. Pointing it at a database some other migrator
owns means two tools fighting over one `user_version` — it will refuse to
open, or it will be refused. A dedicated file is the whole contract.

## Multiple handles over one file: supported and tested

Two `SqliteAssistantStore` instances on the same path — two worker processes,
or two connections in one process — are two connections contending for
SQLite's single write lock. `BEGIN IMMEDIATE` plus a busy-wait strategy is
what makes them wait for each other rather than fail on each other:

- **Synchronous transactions** wait *inside* SQLite via `PRAGMA busy_timeout`
  (default 5000ms) — right when the lock holder is another OS process.
- **Transactions that hold the lock across an `await`** (`claimNext`,
  `AssistantStore.transaction`) instead wait on the **event loop**, because
  the holder may be this same process's *other* handle, and parking the thread
  SQLite would park is the only thread that could ever release that lock.
  Measured against a real two-handle claim: the thread-parking version stalled
  5293ms and then failed; the event-loop version resolved the same contention
  in 4ms.

One gap remains, deliberately unfixed: a synchronous transaction on one handle
cannot event-loop-wait for another handle's in-flight *async* claim in the
same process. See
[ADR 0006](../../docs/adr/0006-hardening-tranche.md).

## Not for a distributed deployment

Multiple handles over one *file* is not multiple *machines*. A distributed or
multi-tenant deployment needs adapters over a networked backend (Postgres,
Redis, or similar) implementing the same ports — see
[`docs/non-goals.md`](../../docs/non-goals.md).

## Transaction isolation caveat

`transaction()` gives atomicity, not isolation. Over `bun:sqlite`, concurrent
store calls issued while a transaction callback awaits genuinely-async work
JOIN that transaction and roll back with it — the connection has exactly one
open transaction, and every statement issued on it belongs to that one. Keep
transaction callbacks free of foreign async work: await the model, the
applier, or another subsystem *outside*, then pass the results in.

## Fencing and CAS

A lease-guarded write reads the task's current lease inside the same
transaction, then performs its `INSERT`/`UPDATE`, and rejects on mismatch with
`LeaseLostError`. Compare-and-set operations (`transitionTask`,
`ProposalStore.transition`) use a guarded
`UPDATE ... WHERE id = ? AND status = ?` and check the driver's reported
`changes` count as the backstop against a race the initial `SELECT` could not
see.

## Priority aging

`SqliteAssistantStoreOptions` extends `TaskAgingOptions` from
`@agentkit/host`. `claimNext` expresses the aging formula in SQL so the
`ORDER BY` sees it, rather than re-sorting a page of rows chosen by the wrong
key. With the default `agingBonus = 0` the term folds to zero and the ordering
is plain `priority DESC, enqueued_at ASC`. See
[ADR 0003](../../docs/adr/0003-task-dependencies-and-subagents.md).

## Schema (v5)

Single-file DDL in `src/schema.ts` (`SCHEMA_V5`), applied idempotently
(`CREATE ... IF NOT EXISTS`, `INSERT OR IGNORE`). No migrations ship — see
"It owns its database file" above.

v4 added conversation branching: `messages` gained `parent_message_id` (a
self-FK), `depth`, `branch_index` and `active`, plus the two indexes those
reads want (`(chat_id, active, depth)` for the active path,
`(parent_message_id, branch_index)` for siblings). A chat is a tree; `active`
is the per-message flag marking which root-to-leaf path through it the
conversation currently is.

v5 added multimodal message bodies: `messages.content_format` (`'text'` |
`'parts'`, default `'text'`) says how to read the `content` column. A string
body is written verbatim, exactly as every pre-v5 row was; a
`AiContentPart[]` body is JSON in the same column. A format tag rather than a
`JSON.parse` guess, because a user message whose text happens to look like a
parts array is a string, and a store that guessed would promote it on the
next read.

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

- Queue state lives on `tasks` + `leases`; there is no separate queue table, so
  nothing can drift out of sync. `tasks` has no `chat_id` column — a task of an
  arbitrary kind has no conversation, so whatever a kind needs (a chat turn's
  `chatId` included) rides in the `payload` column instead. `parent_task_id`
  (lineage) and `depends_on` (a JSON array of task ids, the claim gate) are
  separate columns, neither a foreign key — `createTask` proves both point at
  existing rows before it writes, and an FK would additionally block deleting
  an old completed task a finished row still names. `progress` is an
  overwritten JSON snapshot, never appended to.
- The idempotency guarantee for model-issued writes is a partial unique index:
  `UNIQUE(scope_key, action_id) WHERE action_id IS NOT NULL AND status NOT IN
  ('rejected', 'invalidated')` — see
  [`docs/ports.md`](../../docs/ports.md#proposalstore).
- JSON-shaped fields (`payload`, `metadata`, `envelope`, `operations`,
  `warnings`, `tool_calls`, `failed_ops`, `extra_headers`, `decision`, ...) are
  stored as `TEXT`; the store (de)serializes them, SQLite never inspects their
  contents.

## License

MIT
