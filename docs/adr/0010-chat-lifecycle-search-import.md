# ADR 0010 — Chat lifecycle, search, and transactional import

**Status:** accepted, implemented (2026-09-01; `chat_busy` hardened
2026-09-02)
**Contract impact:** Additive under `CONTRACT_VERSION` `0.4.0` (already
bumped by [ADR 0009](0009-content-parts-attachment-resolver.md)) — new
`ConversationStore`/`TaskStore`/`ProposalStore` port methods, new `MessageDto`
fields, and REST v1 routes (`updateChat`, `deleteChat`, `searchMessages`;
`regenerateMessage` and the management surface follow in the same wave, see
the CHANGELOG). No further `CONTRACT_VERSION` bump, per policy: additive
changes never bump the minor while `0.x`.

## Problem

A chat, once created, could not be renamed, archived, or deleted, and
`listChats` had no filters beyond the default listing. There was no way to
**import** an existing conversation's history — meaning migrating either
target consumer's existing chat data into AgentKit was impossible outright —
and no way to search across a chat's or an assistant's message history. The
originating plan's consumer inventory named this bluntly: "No chat
update/delete/archive, no `listChats` filters, no search, no regenerate API,
no bulk import (history migration impossible without it)."

## Evidence

- OneMind's own FTS pattern — external-content FTS5, triggers, `bm25`
  ranking, and its query sanitizer — as the reference implementation, the
  same reference lineage [ADR 0007](0007-conversation-branching-fork.md) drew
  on for the branching model.
- A Phase B/C fresh-context verifier pass drove exactly the hazard class
  [`docs/ports.md`](../ports.md) now documents by name — **check-then-act
  across an `await` inside a transaction**. It ran
  `ConversationService.deleteChat` concurrently against `claimNext` and got a
  `claimNext` claiming a task the already-committed `deleteChat` transaction
  had deleted. In a single-event-loop host, a concurrent store call
  **flattens into the in-flight async transaction** rather than opening its
  own — so an invariant checked before an `await` and acted on after it is
  not atomic no matter how well-wrapped the surrounding transaction looks.

## Decision

1. **`ConversationStore` lifecycle operations.** `updateChat(chatId, {
   title?, metadata?, archived? })` (`metadata` **replaces**, the same rule
   and rationale as `updateMessage`, so "unset this flag" stays expressible);
   `deleteChat(chatId)` (removes the chat and every message — off-path
   branches included — transactionally); `ChatRecord.archived` as a real
   column, not a `metadata` key, so a store can index it — `false` on every
   created chat **and** on every fork, since a copy of an archived chat is a
   new conversation somebody just made. `ListChatsOptions` gains
   `includeArchived` (default `false`) and `ids` (an exact-id **batch fetch**
   that resolves archived chats regardless — archiving hides a chat from
   *browsing*, not from a caller who can already name it).
2. **`importConversation(input)` — the history-migration primitive** this ADR
   exists to unblock. Writes a whole conversation in one transaction,
   **preserving the caller's ids** — the caller owns identity, the parent
   links and the `active` flags; the store still assigns `orderKey`, `depth`
   and `branchIndex` by the same sibling rules every append follows.
   Validated in full before any write (all-or-nothing), against the same
   shared, pure `planImportedMessages` tree planner the branching machinery
   already uses, so both reference adapters accept exactly the same payloads
   and a migration script written against one is correct against the other.
3. **`searchMessages(query, { chatId?, limit? })`** — an **optional** port
   method, graded behind a `capabilities.search` flag on the conformance
   harness rather than required of every adapter. `sqlite`: FTS5
   external-content table + triggers + `bm25` ranking, porting OneMind's
   sanitizer; `memory`: substring matching, ranked by occurrence count. Both
   index **all** of a message's text parts (`searchTextOf`), not just the
   first — closing the exact bug this ADR's evidence names in OneMind's own
   implementation. `internal: true` and `placeholder: true` records are
   excluded, both flags read at **query** time (metadata is rewritten after
   the fact, so indexing them once at write time would go stale). Hits are
   **not** restricted to the active path — a message on an abandoned branch
   is still something that was said in this chat, and is exactly what "where
   did I see that?" is looking for.
4. **`listMessages` gains `beforeOrderKey`**, paging **backwards** (the
   `limit` active-path messages strictly below the key, still ascending) —
   the load-latest-page shape a chat UI needs on open, alongside the
   existing forward `afterOrderKey` cursor. Setting both together is
   `invalid_cursor`: a range read is a third operation with its own undefined
   answer to "which end does `limit` count from."
5. **Deletion is scoped correctly across the three stores that reference a
   chat, and "what a delete means" lives at the host layer, not any one
   store.** `TaskStore` gains `listByScope`/`deleteByScope`; `ProposalStore`
   gains `deleteByChat` (by `chatId`, **not** `scopeKey` — two chats
   routinely stage writes into one shared document, so a scope-keyed delete
   would silently take a bystander's staged writes). New host-layer
   `ConversationService.deleteChat` runs one `store.transaction`: read the
   chat, read `TaskStore.listByScope`, refuse with `ChatBusyError`
   (`chat_busy`, HTTP 409) if anything is `running` or `waiting_approval`,
   then delete across all three stores. `queued` is deliberately **not** a
   busy status — nothing has been spent on it, and refusing would make a
   chat undeletable for as long as anything sat behind it. Force-cancelling
   live runs is left as a different, explicit, separate caller operation.
6. **The busy check is a fast path only; the guarantee moved into the
   store** after the verifier finding above. `TaskStore.deleteByScope` now
   re-checks `running`/`waiting_approval` and refuses **as part of the same
   synchronous statement or transaction** that performs the deletes — no
   `await` between the check and the act, closing the hazard class named in
   Evidence. `ConversationService.deleteChat` still checks first (so it can
   refuse *before* it has already deleted the conversation half of a chat),
   but the actual guarantee belongs to the store now, and is pinned there by
   the shared chat-lifecycle conformance suite for both reference adapters.
7. **The host's canonical status vocabulary does not grow to express
   consumer-specific UI states.** Designing this lifecycle surface required
   deciding what a run/task "is" for a UI, and both target consumers'
   frontends carry their own richer, pre-existing run-state enums
   (`streaming`/`waiting`/`paused`/`pending`, among others). None of those
   become new `TaskStatus`/`RunStatus` values: `queued | running |
   waiting_approval | completed | failed | cancelled` stays the entire
   vocabulary, and every consumer-specific word is a **client-derived**
   projection over `(status, event log)`, computed once, in one place —
   `waiting` = `queued` with an unresolved dependency ([ADR
   0003](0003-task-dependencies-and-subagents.md)); `streaming` = `running` +
   a `run.started`/delta already on the log; `paused` (OpenPCB's own
   crash-recovery state) has no server equivalent at all — recovery here
   always resolves to a fresh attempt or a dead-letter, never a parked
   "paused"; `pending` never exists, because submit is atomic. This boundary
   is what later makes `runPhase()` in `@agentkit/client` ([ADR
   0013](0013-serving-surfaces.md)) a **pure function** rather than a second
   state machine that has to be kept in sync with the host's.

## Alternatives considered

- **Add the consumer status words as real `TaskStatus`/`RunStatus` values.**
  Rejected: they are views over the same two facts (status + event log)
  computed slightly differently by each consumer; putting them in the store
  would oblige every adapter to compute and persist them consistently, where
  a client-side derivation is cheap and uniform precisely because it happens
  once, over data every server already publishes.
- **A general `listTasks(filter)` instead of the narrow `listByScope`.**
  Rejected: the narrowest query that answers "is anything still live here?"
  is the only question a caller outside `TaskStore` actually has; a general
  filtered, paginated query would owe every adapter statuses, kinds,
  ordering and paging to serve one consumer.
- **Cancel running tasks automatically as part of `deleteChat`.** Rejected,
  deliberately left to the caller: force-cancelling live work and deleting
  history are two different, differently consequential operations, and
  folding one into the other as a side effect would delete a user's data (or
  cancel their work) as a side effect of the *other* decision.
- **Enforce `chat_busy` only in `ConversationService`, never in the store.**
  Rejected after the verifier finding: a check-then-act across an `await`,
  however well-wrapped in a transaction, is not atomic in a single-event-loop
  host where a concurrent store call joins the in-flight transaction rather
  than opening its own — the guarantee has to live in the one synchronous
  statement that performs the delete.

## Consequences

- A migration script targeting either consumer can be written once against
  `importConversation` and run correctly against either reference adapter —
  the primitive this ADR exists to unblock for `docs/migration/`'s playbooks.
- [`docs/ports.md`](../ports.md) now documents check-then-act-across-an-await
  as a **named hazard class**, not a one-off bug: any future invariant of
  that shape must be enforced by a single synchronous statement or
  transaction inside the adapter, never a check in the orchestration layer
  with an `await` between the check and the act.
- [`docs/roadmap.md`](../roadmap.md)'s P5b entry (search + forward paging)
  moves to Done, referencing this ADR.
- The sqlite reference adapter moves to `SCHEMA_V6` in this ADR (chat
  lifecycle + FTS5), on top of `SCHEMA_V5` from [ADR
  0009](0009-content-parts-attachment-resolver.md).

## Out of scope (deliberate)

Full-text ranking tuning beyond `bm25` defaults; a shared search-semantics
contract reconciling `memory`'s substring match against `sqlite`'s tokenized
FTS5 (recorded as an open alignment question, see
[`docs/roadmap.md`](../roadmap.md)'s Later list); branch-level (as opposed to
whole-chat) archive or delete — still deferred from [ADR
0007](0007-conversation-branching-fork.md); automatic cascade-cancel of
running tasks on delete.
