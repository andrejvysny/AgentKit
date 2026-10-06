# @agentkit/adapters-sqlite

**The durable `AssistantStore` for a single-process host: every
`@agentkit/host` port over Bun or Node SQLite, with real transactions, lease
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

## Bun and Node drivers

The default entry point uses Bun's built-in `bun:sqlite`. Node 22 or newer
uses the explicit `@agentkit/adapters-sqlite/node` entry point and the optional
`better-sqlite3` native dependency. The published umbrella package exposes
these as `agentkit/adapters-sqlite` and `agentkit/adapters-sqlite/node`.

```ts
import { NodeSqliteAssistantStore } from "agentkit/adapters-sqlite/node";

const store = new NodeSqliteAssistantStore("./data/agentkit.sqlite");
```

Both drivers share every store implementation, the transaction gate, schema
checks, and migrations. Their database files are interchangeable, including
the FTS5 index. Install optional dependencies when packaging a Node host;
applications that relocate native binaries can provide `nativeBinding` in
`NodeSqliteAssistantStoreOptions`. Importing the Node entry point never loads
`bun:sqlite`; importing the default entry point still requires Bun.

Bindings preserve null and blob values and convert booleans to SQLite integers.
Stored integers outside JavaScript's safe integer range are rejected rather
than rounded. Node accepts `$`, `:`, and `@` named parameter prefixes, but
rejects a binding bag containing several prefixes for the same name.

## It owns its database file

**Give it a path nothing else manages.** The store applies its own schema on
open and guards the file with `PRAGMA user_version`. It initializes an empty
file, recognizes the complete original v8 table layout, and applies registered
incremental migrations in one `BEGIN IMMEDIATE` transaction. Unsupported
versions, foreign `application_id` values, extra foreign tables, malformed
baseline tables, and integrity failures are refused before persistent writes.
The original v8 layout has no application marker and remains supported with
`application_id = 0`. Valid FTS5 shadow tables are recognized explicitly.
Pointing the adapter at a file managed by another migrator is unsupported.

**The current version is 9** (`SCHEMA_VERSION` in `src/schema.ts`). The v8-to-v9
migration adds private `provider_run_scopes` and `provider_continuations` tables.
Original v8 files upgrade transactionally; fresh databases initialize the same
immutable v8 baseline and apply that migration. Migrated table definitions are
checked on reopen, so a version marker alone cannot authorize a foreign layout.
Continuation state is limited to 1 MiB UTF-8 and 1024 input items, bound to a run's
provider/model/connection generation/chat/branch, and written only under its live
lease. It is never stored in messages, run events, or REST DTOs. Task, message,
and chat foreign keys remove obsolete state automatically.
Each bound run starts with `state_required = 0`, so a failed or cancelled request
that never saves state can be followed normally. A successful save writes state
and sets the marker to 1 in the same transaction. Missing state thereafter fails
closed with `provider_continuation_state_missing`. Chat or anchor deletion retains
that marker while the task exists; task deletion removes the binding and marker.
Every saved anchor must belong to both the bound chat and run.

Version 8
added `idx_messages_run ON messages(chat_id, run_id, depth, order_key)` — the
whole `ORDER BY` of `lastMessageOfRun`, which crash recovery reads on every
resumed attempt, so the plan needs no temporary b-tree over the message table —
and `proposals.claimed_at`, the instant an apply claim was taken, which
`reconcileInterrupted`'s staleness window measures against (see [ADR
0014](../../docs/adr/0014-hardening-tranche-2.md)). A file written by version 7
is **refused**, because no migrations exist for pre-v8 development schemas.
Use a compatible release or restore a verified backup. The adapter never
deletes, recreates, or silently downgrades a refused database.

The production registry contains the v8-to-v9 step; v8 remains the minimum
supported baseline. Each step advances exactly one version. A failed step rolls
back DDL, data, and `user_version`; a subsequent open can retry it. The version
is stamped only after all steps and integrity checks pass. Schema refusal or
migration failure closes the newly opened handle.

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

Concretely: `SqliteTaskStore.claimNext` holds its transaction across `await`s;
a second handle's synchronous transactions (`appendEvents`, `transitionTask`,
`endAttempt`, every `withTx`) can only park the thread while they wait for
that lock, which is the one thread that could run the holder's continuation
and commit — so a fully concurrent claim-AND-execute workload over two
handles in one process stalls for the whole `busy_timeout` and then throws
`SQLITE_BUSY`. Pinned, not fixed, by the skipped case at the bottom of
`tests/sqlite-multi-handle.test.ts` (seed 7 over a 4-worker/24-task/40-step
schedule reproduces it in ~5s). The fix is architectural — either the
synchronous transaction helpers become async, or `claimNext` stops holding
its transaction across `await`s — and is tracked as the roadmap's
"claim-tx-across-awaits redesign" item.

## Not for a distributed deployment

Multiple handles over one *file* is not multiple *machines*. A distributed or
multi-tenant deployment needs adapters over a networked backend (Postgres,
Redis, or similar) implementing the same ports — see
[`docs/non-goals.md`](../../docs/non-goals.md).

## Transaction isolation caveat

`transaction()` gives atomicity, not isolation. The connection has exactly one
open transaction and every statement issued on it belongs to that one, so
WRITES from other callers — a second `transaction()`, a worker's `claimNext`,
an ordinary `updateChat`, and a `SqliteMcpServerConfigStore` sharing the handle
— wait for it and then run in a transaction of their own rather than joining
one whose rollback would erase them. Durable task, event, projected-message, and
private-continuation reads also wait for unrelated transactions to commit; their
transaction-owner views can read their own writes.

**The queue is FIFO across both kinds.** A transaction and a root write take a
slot in the same queue when they are issued, and run in that order. Re-checking
"is the connection free yet?" after each wait instead is a retry loop, not a
queue, and it starves: transactions issued *after* a waiting write are already
chained to the promise that write is waiting on, so they run first — for as
long as they keep arriving.

The corollary is that a callback must work through the `tx` it is handed: a
call issued on the ROOT store from inside it is indistinguishable from an
unrelated caller's, so it waits for the transaction it is running inside. That
wait is bounded by `transactionGateTimeoutMs` (default 30s) and ends in
`TransactionGateTimeoutError` (code `transaction_gate_timeout`) instead of
hanging — the budget is a watchdog, never a knob to raise. Keep transaction
callbacks free of foreign async work: await the model, the applier, or another
subsystem *outside*, then pass the results in.

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

## Schema (v9)

The immutable baseline DDL in `src/schema.ts` (`SCHEMA_V8`) is applied idempotently
(`CREATE ... IF NOT EXISTS`, `INSERT OR IGNORE`). The transactional migration
registry then adds the private v9 continuation tables — see "It owns its database
file" above. Unsupported older or future versions are refused without reset,
downgrade, or destructive recovery.

Original v8 databases remain supported. A file stamped `user_version = 7` is
refused with `sqlite_schema_version`; use a compatible release or a verified
backup. No destructive recovery runs automatically.

v8 adds two durability follow-ups: `idx_messages_run` on
`messages(chat_id, run_id, depth)` — `lastMessageOfRun`'s whole query, the
lookup that links a resumed turn to its own chain — and `proposals.claimed_at`,
the instant an apply took the `approved → applying` claim.
`ProposalService.reconcileInterrupted({ staleAfterMs })` keys its window on
that column: every stamp it had before (`applied_at`, `decided_at`,
`created_at`) is older than the claim for a write a human approved and
something applied later, so a live apply could look stale and be reconciled out
from under itself. Nullable — a row that never reached `applying` has no claim
instant.

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

v7 adds one table, `mcp_servers` — the durable half of `McpServerConfigStore`
(`@agentkit/mcp-client`), served by the standalone `SqliteMcpServerConfigStore`
rather than by a seventh member of the `AssistantStore` aggregate: an MCP
server config shares a transaction with nothing, and folding it into the
aggregate would make every hand-rolled store implement a port most hosts never
use. The table lives in this one DDL anyway, because the file is one database
with one `user_version` and a second DDL string applied by a second constructor
is a second thing that can be forgotten. `alias` is `UNIQUE` (BINARY collation,
so case-sensitive — the alias grammar has no uppercase in it) because it is the
tool namespace every canonical id embeds; `enabled` is nullable, since an absent
`McpServerConfig.enabled` means "default true" and is a different record from a
stored `true`. No secret material: `secret_refs` holds `SecretStore` **refs**.

v6 added chat lifecycle and full-text search:

- **`chats.archived`** (`INTEGER NOT NULL DEFAULT 0`) plus
  `idx_chats_archived_updated ON chats(archived, updated_at DESC)` — the
  default `listChats` query. A real column rather than a `metadata` key,
  because the listing filters on it and an index cannot reach into a JSON bag.
- **`message_search_source`** — a VIEW computing the searchable text of every
  message: a `'text'` row is its `content` column; a `'parts'` row is **all**
  of its text parts, `group_concat`'d with a newline (`json_each` +
  `json_extract`). This is `searchTextOf` from `@agentkit/host`, expressed in
  SQL. *All* parts, never just the first: indexing only part one is a real bug
  in the system this design is copied from, and it fails silently.
- **`message_search`** — an `fts5` virtual table with
  `content='message_search_source'`, `content_rowid='rowid'`,
  `tokenize='unicode61 remove_diacritics 2'`. **External content**, so the
  index holds terms and postings and never a second copy of the message text;
  a plain FTS5 table would double the size of the only table holding user
  prose. `snippet()` re-reads a body through the view when a hit needs one.
- **Three triggers** (`messages_search_insert` / `_delete` / `_update`).
  External content maintains nothing on its own, so every write to `messages`
  tells FTS5 what changed; the `'delete'` command carries the OLD projection,
  recomputed from `old.*` because the view's row is already gone by then. The
  same generator emits the projection for all three aliases, so an index and a
  delete-trigger cannot drift.
- **A guarded backfill**, so re-applying the DDL on every open cannot
  double-index. The guard reads the `%_docsize` shadow table, not
  `message_search` itself: scanning an *external-content* FTS5 table without a
  `MATCH` iterates the **content source**, so `SELECT 1 FROM message_search` is
  non-empty whenever `messages` is — the obvious guard would read "already
  populated" on a completely empty index and skip the work it exists to do.
- Queries filter `internal`/`placeholder` at **query** time, via `json_extract`
  on the joined `messages` row, because both are `metadata` flags that get
  rewritten (a placeholder becomes a real answer when its run finishes) and an
  index that decided at insert time would keep finished answers unfindable.
  Query text is sanitized before it ever reaches FTS5 (`toFtsQuery`): the
  operator characters `"`, `*` and `^` are stripped, parentheses become spaces,
  and each surviving token is re-emitted as a quoted phrase — which neutralises
  `-`, `:` and the `AND`/`OR`/`NOT`/`NEAR` keywords, and turns two typed words
  into FTS5's implicit AND. Nothing left after sanitizing means no hits, not an
  exception.

| Table(s) | Port |
|---|---|
| `chats`, `messages` | `ConversationStore` |
| `message_search_source` (view), `message_search` (fts5) + 3 triggers | *not port records* — the derived search index behind `ConversationStore.searchMessages`. |
| `tasks`, `task_attempts`, `leases`, `task_events` | `TaskStore` |
| `proposals`, `proposal_outcomes` | `ProposalStore` |
| `providers`, `provider_models`, `provider_capabilities` | `ProviderStore` |
| `settings` (single row, `id = 1`) | `SettingsStore` |
| `mcp_servers` | `McpServerConfigStore` (`@agentkit/mcp-client`) — served by the standalone `SqliteMcpServerConfigStore`, not by the aggregate. |
| `outbox` | `OutboxStore` |
| `provider_run_scopes`, `provider_continuations` | Private `ProviderContinuationStore`; never REST DTOs. |
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

## VACUUM caveat

**Do not run `VACUUM` against a live AgentKit database file.**

`message_search` is an **external-content** FTS5 table
(`content='message_search_source'`, `content_rowid='rowid'`), so every posting
in it is keyed by a `messages.rowid`. `messages` has a `TEXT` primary key and
therefore no `INTEGER PRIMARY KEY` column, which puts it squarely in the case
SQLite warns about: *"if [a] table does not have an INTEGER PRIMARY KEY column,
then the VACUUM command may change the rowids of entries"*
([sqlite.org/lang_vacuum.html](https://www.sqlite.org/lang_vacuum.html)).
Renumbering rewrites the content table without telling FTS5, so the index goes
on pointing at rowids that now belong to *other* messages: `searchMessages`
starts returning the wrong message for a hit, and `snippet()` cuts a window out
of a body that never contained the term. Nothing raises, and no later write
repairs it — the triggers only maintain rows that change *after* the fact.

Nothing in this adapter emits `VACUUM`; this caveat is about a DBA, a backup
script, or a "compact the database" button reaching for the file. If one has
already run, back up the file and rebuild the index from the content table
using the DDL's backfill through the ordinary SQL path:

```sql
INSERT INTO message_search(message_search) VALUES('delete-all');
INSERT INTO message_search(rowid, body)
  SELECT source.rowid, source.body FROM message_search_source AS source;
```

Note that FTS5's own `VALUES('rebuild')` command does **not** work here under
`bun:sqlite`: rebuilding re-reads the content source from inside the fts5
extension, where the JSON functions the view depends on are not resolvable, and
it fails with `no such table: main.json_each`. The two statements above do the
same job through the ordinary SQL path, which is why the schema's backfill is
written that way in the first place.

## License

MIT
