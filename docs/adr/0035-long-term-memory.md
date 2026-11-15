# ADR 0035: Long-term memory

Status: accepted (2026-11-15). Implements MEM-01 of the memory and retrieval
spec (roadmap package 3.1). Builds on ADR 0021 (layered crates) and ADR 0024
(repository ports).

## Context

Memory is a table of dated, attributed statements: a record exists because the
owner typed it, asked for it or approved it, and keeps its evidence. Search
fuses full-text and vector ranks. The storage target is SQLite (FTS5 and
sqlite-vec) in the AgentKit database, but the domain rules - the write gate,
supersession, rank fusion - must not depend on it, and a different vector
store must be able to replace the adapter without changing ranking.

## Decision

1. **A new layer-3 context, `agentkit-memory`,** holds the domain types
   (`Record`, `NewRecord`, `Scope`, `Kind`, `Status`, `Origin`, `MemoryEvent`,
   filters, `Corpus`), the four ports and `MemoryService`. It has no SQL and
   names no store. `check-arch` already lists it as a context that may depend
   on engine and sessions only; the crate depends on neither (it needs no
   engine or session types), and the allowlist does not grow.
2. **Four ports, as in the spec.** `MemoryRepository` (records and the audit
   trail), `RetrievalIndex` (ids, filter fields and lexical/vector search),
   `Embedder`, `Reranker`. Ports are object-safe and the errors are one crate
   type, `MemoryError`, not an adapter-typed associated error: a memory call
   has no lease or session semantics a context must branch on, and the
   runtime holds the ports as `Arc<dyn ..>`.
3. **Transactions are opaque handles, begun and committed on the port.**
   `MemoryTx` and `IndexTx` wrap the adapter's own state and are distinct
   types, so a SQLite adapter can share one transaction between them and a
   split store can implement an outbox. Dropping a handle is a rollback.
   Departure from the spec's signatures: `begin`/`commit` (and
   `begin_index`/`commit_index`) exist on the ports, and `set_status` and
   `purge` take the time (and, for purge, the actor) as arguments, because the
   domain never reads a clock.
4. **Supersession is one repository operation.** `supersede(old, new)` inserts
   `new` with `supersedes`, closes `old.valid_to` at `new.valid_from` and sets
   `old.superseded_by`, and appends the events, in one transaction. `old` must
   be a current active record in the same scope.
5. **Purge is verifiable.** It deletes the row, scrubs the `detail` of its
   earlier events (the `purged` event keeps id, time and actor) and returns a
   `PurgeReport` with rows removed per table and the residue left for those
   ids. `RetrievalIndex::purge` does the same for index items and vectors.
6. **The in-memory twin of each port and one conformance suite per port live in
   `agentkit-testkit`** (ADR 0024 decision 6): `InMemoryMemoryRepository`,
   `InMemoryRetrievalIndex`, the deterministic hash-based `HashEmbedder`, and
   suites for the repository, the index, the embedder and the reranker. The
   suites are generic over the implementation; the SQLite adapter (package
   3.2) runs the same ones. `NoopReranker` is the default reranker and ships in
   the crate.
7. **The runtime builder takes the ports,** `memory(repository, index)`,
   `embedder(..)`, `reranker(..)`; `AgentRuntime::memory()` exposes the
   service. An embedder or reranker without `memory(..)` is refused at
   `build()`. The consumer names the concrete adapters, as it does the store.

## Consequences

- `IndexItem` carries the lexical `text`, which the spec's sketch left in the
  source table. An adapter with an external-content FTS table ignores it; the
  in-memory twin has no source table and must keep it.
- `IndexItem.text` exists for twins and adapters without a source table;
  a real adapter with external-content FTS may ignore it.
- Only active records are indexed (the service enforces it: proposals are
  never upserted, rejection and retraction delete). Supersession marks the old
  item `Searchability::Superseded` in the same write path; default search
  hides it, `IndexFilter::include_superseded` shows it.
- Purge also scrubs the `detail` of other records' events that name a purged
  id (`PurgeReport::neighbour_events_scrubbed`). Purging a successor leaves the
  predecessor closed (`valid_to` stays, `superseded_by` is cleared): forget
  removes the update; the earlier record stays as history.
- The in-memory index folds case and common Latin diacritics to match FTS5's
  `unicode61 remove_diacritics`; the suite pins that behaviour, and SQLite is
  the reference where they disagree.
- The suites do not pin concurrent transactions: a serializing adapter may
  block a second `begin`. The twin is optimistic (stale commit returns
  `MemoryError::Conflict`); that is implementation behaviour, not contract.
- The write gate, hybrid search with rank fusion, the core block and the
  framework tools are later packages; they land in this crate against these
  ports.
- SQLite adapters (roadmap 3.2, schema v21): `SqliteMemoryRepository` and
  `SqliteRetrievalIndex` in `agentkit-store`. Both stage a transaction (the
  worker owns the only connection) and replay it in one SQLite transaction at
  commit; `MemoryRepository::commit_with_index` lets a repository that
  recognises its own index apply both queues in one, and the service uses it
  for every write. Only active records are indexed below the service too: a
  commit refuses to index a non-active record and a status change away from
  active deletes its index row in the same transaction. The FTS5 table `memory_fts` is external-content over the
  index's own item table (`memory_index_items`), not over `memory_records`,
  because items are addressed by id and need not be records (the indexed
  text is the item's own copy, which purge must remove too). Timestamps are
  Unix milliseconds, not ISO text.
- Purge runs under `secure_delete`, merges the FTS5 segments, and checks for
  residue (item rows, FTS docsize rows, FTS matches of the purged text). The
  WAL is checkpointed (`PurgeReport::wal_checkpointed` /
  `wal_pages_pending`; an incomplete checkpoint is not clean) and an
  incremental vacuum requested, best-effort. FTS5 `secure-delete` zeroes
  postings only on segment merge, so every purge runs `optimize`. Fresh
  databases are created with `auto_vacuum=INCREMENTAL`; migrated databases
  keep their mode (no forced VACUUM, which rewrites the whole file), so the
  vacuum is a no-op there and freed pages stay zeroed by `secure_delete`
  rather than reclaimed.
  `MemoryService::purge` calls the repository and then the index;
  `sweep_retracted` purges records 30 days after their `retracted` event and
  rejected proposals 90 days after `rejected` (`REJECTED_RETENTION_MS`), and
  `AgentRuntimeBuilder::sqlite_memory()` runs it at build and hourly.
- Vectors and hybrid search (roadmap 3.5). Schema 22 (v21 is untouched)
  adds `embedding_models.retired_at` and `memory_vectors`: ONE BLOB PER
  (model, item), stored unit-length, removed with the item by cascade and by
  explicit delete in purge (counted as `index_vectors`, residue-checked).
  This deviates from the spec's per-model `vec0` table: the BLOB table is
  the source of truth, is portable, needs no extension, and keeps cascade
  deletes and "which items lack a vector" as plain SQL. `knn` is an exact
  scan driven from the filtered items (scope index first) with a bounded
  top-k heap. With the store's `vec` feature (OFF by default; `sqlite-vec`
  `=0.1.9`) the same rows are MIRRORED into one `vec0` table per model,
  partitioned by `scope_key` and keyed by the item's `seq` rowid; it
  answers `knn` per scope partition and widens its fetch until `k` rows
  survive the other filters. `retrieval_index::run` is the conformance
  suite for SQLite and the twin.
  `MemoryService::search` requires at least one scope key
  (`MemoryError::EmptyScope`), clamps `limit` to 1..=8 (`0` means 5), fuses
  lexical and vector top 50 by reciprocal rank (k = 60, equal weights),
  reranks the top 30 through the `Reranker` port, then re-reads every hit
  from the record store: only active records are returned (superseded ones
  only with `include_superseded`), valid at `as_of` when set (the index
  filters on it too, via the record's validity window). Hits are tagged
  `lexical | vector | both`, in mode `hybrid` or `lexical_only` (no
  embedder, no active model for it, or a failed vector side); the active
  model is a point lookup (`RetrievalIndex::active_model`).
  Embedding is background work: writes only flag the service, and the
  runtime's indexer (weak handle, errors logged, exponential backoff to 5
  min) calls `index_pending`. A fill writes VECTORS ONLY through
  `RetrievalIndex::put_vectors`, which skips an item that is gone, whose
  text hash changed, or (SQLite) whose record is not active, inside one
  transaction: a purge or retraction racing a fill can neither resurrect an
  item nor reset its `searchable` state. Indexing a memory-corpus item
  requires an active record (a missing one is refused); `commit_with_index`
  replays record ops before index ops. A model change is `begin_reembed`,
  `fill_model`, a final fill under the service write gate, then
  `commit_reembed(at)`, which stamps `retired_at` from the caller's clock;
  `drop_retired` removes the retired model `REEMBED_GRACE_MS` (24 h) later.
  A `building` model left by a crash is resumed by `index_pending` or
  dropped by `abort_reembed`; a configured model that differs from the
  active one with no re-embed is reported by `embedding_status`
  (`Mismatch`). `agentkit-embed-local` holds the ONNX embedder behind its
  `onnx` feature (OFF; `ort` `=2.0.0-rc.15` with `load-dynamic`, so the host
  supplies libonnxruntime via `ORT_DYLIB_PATH` and nothing is downloaded at
  build time; `tokenizers` `=0.23.2`): the model and tokenizer sha-256 are
  pinned, the bytes are read once, hashed, and loaded from that buffer.
