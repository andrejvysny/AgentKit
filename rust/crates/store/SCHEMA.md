# Native database and event sequences

Native databases use application ID `0x414b5253` and schema version `2`. They are
independent of the TypeScript SQLite schema. Foreign application IDs, newer
versions, and incompatible schema signatures are refused before persistent
journal or synchronization settings are changed.

An exact known version-1 schema migrates transactionally to version 2. Only the
event sequence index changes: `INTEGER CHECK(seq>=0)` becomes
`REAL CHECK(seq>-1)`. Existing event JSON, tasks, attempts, leases, fencing counter,
outbox records, conversations, proposals, receipts, and settings are preserved.
Migration does not claim, recover, resume, or infer a task outcome. A malformed
version-1 schema is refused before migration begins.

The generic event protocol follows `TaskEventEnvelopeSchema`: finite JavaScript
numbers increase strictly from an initial previous value of `-1`. The first event
may have a fractional or negative sequence above `-1`; gaps and large finite
numbers are valid. String fields have no extra nonempty or timestamp-format
restriction. Required fields and their actual schema types are checked before
event publication. Each append batch, its outbox records, and any message
projections commit atomically. Duplicate sequences, regressions, and duplicate
event identities leave no committed prefix.

Use `events_after_number` for a numeric cursor and `next_sequence` for the generic
JavaScript `last + 1` successor. At large magnitudes binary64 addition may return
the same number, as in JavaScript; a caller must still choose a strictly greater
sequence for publication. `events` retains an integral-cursor convenience API.
`next_seq` is a checked convenience for native generated runs: it requires a safe
nonnegative integral successor. The native core emitter continues to generate
contiguous zero-based integers.

## Message search (schema version 9)

Search is answered by a persistent FTS5 index, the counterpart of the TypeScript
adapters' `message_search`: a `message_search_source` VIEW projects each message's
searchable body (a string content as itself, a parts content as all of its
`{"type":"text"}` parts joined by newlines), an external-content FTS5 table
(`unicode61 remove_diacritics 2`) keyed by `messages.rowid` indexes it, and three
INSERT/UPDATE/DELETE triggers on `messages` keep the two in step — every message
write, including the TypeScript import's direct `INSERT ... ON CONFLICT` and the
chat-deletion FK cascade, flows through them. Migrating a pre-v9 file backfills the
rows that predate the index. `search_messages` answers `message_search MATCH`
ranked by bm25; the chat/`archived`/`metadata.internal`/`metadata.placeholder`
constraints are applied at query time in the join, never at indexing time, because
those metadata keys are rewritten after a row is indexed. The index is keyed by
`messages.rowid`, so a live store file must never be VACUUMed (SQLite may renumber
rowids and silently orphan every posting).
