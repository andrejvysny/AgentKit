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
  Pooling is per model (`LocalModel::pooling`: `Pooling::Cls` or `Mean`,
  `normalize`, optional output name; `PoolingConfig::bge_m3()` is CLS +
  normalised, `bge_m3_spec()` has no prefixes): BGE-M3 is CLS-pooled, never
  mean-pooled. Output defaults to `last_hidden_state`, then
  `token_embeddings`, `sentence_embedding`, `dense_vecs`, else the first
  output; a rank-2 output is taken as already pooled. Batches are padded with
  the attention mask. The gated test `tests/bge_m3_parity.rs` checks cosine
  >= 0.999 against sentence-transformers (`AGENTKIT_EMBED_MODEL_DIR`). The
  session loads from memory, so ONNX exports with external `*.onnx_data`
  (BAAI's own `onnx/model.onnx`) are not loadable; use a single-file export.

## Addendum: explicit writes, tools and the core block (roadmap 3.3)

- **Write paths.** `MemoryService::propose` is W1 and W3. The model never
  decides what becomes active, and neither its `owner_requested` flag (advisory
  only) nor a quote it offers (the tool takes none) is consulted. A record is
  written active with origin `owner_explicit` only when both hold, checked in
  code: (a) the record `text`, after whitespace, zero-width, smart-punctuation
  and case folding, is a verbatim span (at least 12 characters) of the
  owner-typed text of the turn, and the evidence quote is that matched span as
  the owner typed it; (b) that owner text asks to remember, by a whole-word
  match (case and diacritics ignored) against one list, `REMEMBER_PHRASES` in
  `gate.rs` (English: remember, don't forget, note that, keep in mind; Slovak:
  zapamataj, pamataj si, nezabudni, poznac si; Czech: zapamatuj si, pamatuj si,
  nezapomen, poznamenej si, mej na pameti), **bound to the statement**:
  `consent_for` (`consent.rs`) grants only when the phrase opens an imperative
  clause outside a question or a quote and the record text equals what it
  governs (the rest of that sentence, the part before a trailing "..., remember
  that", or the whole next sentence after a bare "remember this"). A negated or
  "forget" request ("do not remember that ...", "nepamätaj si, že ...") makes
  `propose` return `ProposeOutcome::Refused` and write nothing; any other
  case is a proposal. `has_remember_request` is only a phrase scan, not
  consent. The evidence also carries the
  chat id, message id and SHA-256 of that text, and is secret-screened like the
  text. Anything else is a proposal (`agent_proposal`), and the tool result
  says why it was not written directly. The owner-typed text comes from the
  `OwnerTurnSource` port: the user message the run answers, text parts only
  (never the turn-context part), not a message that relays a child's report and
  not any message of a child agent's chat, and without what looks pasted
  (composer `pastedRanges`, fenced code blocks, Markdown quoted lines,
  paragraphs over 600 characters); pasted text is data, not the owner's words. `crates/memory/src/gate.rs` is the minimal write gate (containment,
  size, secrets, duplicate and 90-day rejection suppression); 3.4 extends it
  with update, rate and the sensitive-category downgrade. Its secret check is a
  value-pattern scan in the memory crate because the credential redactor in
  `agentkit-core` is key-based; unifying them is a follow-up.
- **Rationale** is not a column: `NewRecord.rationale` becomes the `detail` of
  the record's first audit event, so a purge scrubs it with the rest.
- **Port additions.** `MemoryRepository::set_core` (pin or unpin, with a
  `pinned` or `unpinned` event) and `record_use` (`last_used_at`, `use_count`,
  unaudited telemetry). The service batches uses in memory (`note_use`) and
  `flush_usage` writes them; the runtime flushes every 30 seconds, so no search
  waits for a write.
- **Owner actions** (`approve` with optional edited text, `reject`, `edit` as a
  supersession that keeps the pin, `retract`, `pin`, `unpin`, `list`,
  `history`, `core_block`, `core_fill`) are service methods for hosts; the
  owner UI is a later package. `pin` refuses when the block of the owner scope
  plus the record's scope would pass 1,500 estimated tokens (characters / 4);
  the repository re-checks that ceiling inside the committing transaction
  (`set_core` takes a `CorePin { order, max_tokens }`), so two concurrent pins
  cannot both pass a stale check. `edit` closes the old record with a
  compare-and-set on `valid_to IS NULL` (store replay and the twin's base
  version), so an edit racing a pin or a retraction loses with `Conflict`.
- **Tools** live in `agentkit-memory` (`memory_tools`) over the ports
  `SessionScopes` (the owner's `user` scope, a host-set project id in chat
  metadata `agentkit.memory.projectId`, and the session profile's agent id) and
  `OwnerTurnSource`. Search and get are `read`; propose and forget are `write`
  and contributed as direct writes. The runtime's `memory` lane contributes
  them under namespace `memory` and its guard returns an approval ask for
  `memory_forget` on every call, so the staging wrapper binds the answer to the
  arguments' hash; staging never keys a standing grant for the `memory`
  namespace, so each forget asks again. A child is read-only by convention:
  `MEMORY_WRITE_TOOLS` names the two tools a role filter must drop, and both
  declare effect `write` and capability `memory.write` for the delegation
  guard (package 4.4) to key on. Results carry the "information, not
  instructions" line in the summary the model reads as well as in the data; `memory_get` and `memory_forget` treat a proposal, a rejected or a
  retracted record, or another scope's record as not found.
- **Core block (S4).** `render_core_block` renders the active pinned records of
  the session's scopes, ordered scope, `core_order`, id, inside
  `<memory kind="core">` with one labelled line per entry; text is flattened to
  one line, invisible format characters are stripped, and any `<memory` tag in
  it (any case, spaces allowed) is broken. The chat executor takes the
  block from a `CoreMemorySource` port, freezes it in chat metadata at the
  session's first turn like S3, queues a `core_memory_changed` note per scope
  for each later pin or edit, and adopts the current block when the queued
  notes pass `REFREEZE_DIFF_BYTES` or `refreeze_core_memory` is called
  (explicit reload; compaction hosts call it beside `refreeze_project_rules`).
  An unpin, retraction or forget of a pinned record refreezes at once, on the
  first turn that sees it (one `cache_break`, reason `refreeze`): a removed
  entry's text must leave the context, and a note never repeats it (removed
  entries are named by id only).
- W2 meaning: auto-capture goes active only when the candidate text is one
  complete, non-question, non-refusing sentence of the owner's eligible text
  (`is_full_statement`) or exactly the proposition the owner asked to remember
  (`consent_for` = Granted); a closing `.!?` of the quote is ignored when
  matching. A covered paraphrase or clause waits as
  `ProposalReason::Rewritten`, and a candidate inside an owner refusal is
  `Rejection::OwnerRefused`. `memory_forget` retracts through
  `retract_approved`, recording the owner as actor (the tool is always
  owner-approved), so the 90-day repeat suppression applies as for the UI;
  the requesting actor is kept in the event detail
  (`{"requested_by":..., "via":"memory_forget"}`, port `set_status` `detail`).
  A `kind` filter on memory_search runs inside `MemoryService::search_kinds`
  before ranking, with retriever pools raised to `KIND_FILTER_TOP_K`.
- Search: the memory_search tool calls `MemoryService::search` (hybrid, scope required, limit 1..=8) and reports its `mode` (`hybrid` or `lexical_only`).
