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

## Subagent registry and advisor findings (schema version 10)

`subagents` holds one row per spawned child chat: the parent binding
(`parent_chat_id`, optional spawning `parent_task_id`), the spawn contract
(`role`, `depth`, `prompt`, optional `model`), the child's CURRENT turn task
(`turn_task_id`, re-pointed by the host when a follow-up starts a new turn),
and the projected lifecycle — exactly `queued`/`running`/`completed`/
`failed`/`cancelled`. A terminal row is frozen: a later update names a
different status is refused, while the same-status update only fills
`result`/`error`, so result delivery retry is idempotent. Indexes serve the
per-parent and per-parent-task listings; the orphan reaper enumerates the
non-terminal rows and the concurrency budget counts them.
`advisor_findings` is the advice ledger: one row per observation from the
`explicit` or `watchdog` source, with a CHECK-pinned severity, a JSON-object
`detail`, and an `escalated` flag for findings already surfaced to the user.
Inserts are idempotent on `finding_id`. Model-authored text (`title`, the
strings inside `detail`, and the `subagents` `prompt`/`result`/`error`
columns) is capped with a marked truncation at the write boundary. The
watchdog's rate-limit window counts the REVIEW ledger, not findings — see
version 11. `delete_chat` clears this chat's child-registry row, its roster
as a parent, and its advisor traces in the same transaction.

## Advisor review ledger (schema version 11)

`advisor_reviews` records one row per STARTED review (`run_id`, reviewed
`chat_id`, CHECK-pinned `origin`, `trigger_kind`, `started_at`). It exists
because findings are a dishonest denominator for the watchdog's rolling
window: a sweep that correctly concludes "nothing wrong" persists no
finding, and counting only findings lets an adversarially triggered session
drain provider quota unbounded. `count_advisor_reviews_since` (indexed on
`(chat_id, started_at)`) is the durable budget the watchdog consults before
firing another review.
