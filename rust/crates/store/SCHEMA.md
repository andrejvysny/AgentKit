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
