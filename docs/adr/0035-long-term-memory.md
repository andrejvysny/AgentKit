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
