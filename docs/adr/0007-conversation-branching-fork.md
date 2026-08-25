# ADR 0007 — Conversation branching and transactional chat fork

**Status:** accepted, implemented (2026-08-25)
**Contract impact:** `CONTRACT_VERSION` `0.2.0` → `0.3.0` — additive DTO
fields (`MessageDto.parentMessageId`/`branchIndex`/`active` — `depth` is
deliberately not published, it is derivable from the parent chain;
`SubmitMessageRequest.parentMessageId`) and three new REST v1 routes
(`forkChat`, `activateBranch`, `listSiblings`). Purely additive per the
`CONTRACT_VERSION` policy documented in `docs/contracts.md` — this is a
minor bump, not a breaking one, and goldens were re-recorded in the same
commit (only ids/timestamps/version changed; event shapes are unchanged).

## Problem

`docs/roadmap.md`'s P5a named the goal: a chat is currently
a single linear list of messages, with no way to edit an earlier turn and
try again without losing the original, and no way to branch a conversation
into two independent continuations without duplicating the whole prefix by
hand. Two production-proven mechanisms for this already exist in this
framework's reference lineage (OneMind) and needed to be ported rather than
redesigned: in-chat branching, and whole-chat forking.

## Evidence

OneMind's chat data model and branch/fork services (`src-ts/`), scouted
file:line:

- **Data model** (`db/schema/message.ts:21-73`): `parentMessageId` nullable
  self-FK (root = `null`), `branchIndex` int (auto `max(sibling)+1` in
  `MessageRepository.createBranch`, `db/repositories/message.ts:124-152`),
  `depth` int, `isActive` bool — the per-message `isActive` flag **is** the
  whole active-path representation; no materialized path, no recursive CTE.
  Indexes: `(chatId, isActive, depth)`, `(parentMessageId, branchIndex)`.
- **Active path read** (`message.ts:47-70`): a single flat query —
  `WHERE chatId AND isActive AND NOT deleted ORDER BY depth, createdAt`.
- **`activateBranch`** (`branch-service.ts:133-163`): walks ancestors plus
  active-descendants (an absent active child falls back to `children[0]`),
  then diff-updates `isActive` flags. **Not transactional** (a per-row
  update loop) and the tree-walk itself is **untested** in OneMind.
- **Edit/regenerate** (`message-service.ts:172-331`): edit = a new user
  branch, activated, with an empty assistant draft child created and
  activated under it; regenerate = a new assistant sibling under the same
  user parent; resend = a task retry only, no new branch.
- **Fork** (`chat-manager.ts:284-401`): copies the **active-path prefix**
  up to and including `fromMessageId` only (the point must be on the active
  path; a system-role fork point is refused) — not the whole tree. One
  transaction (Bun sqlite `IMMEDIATE`, `SQLITE_BUSY` retried); fresh UUIDv7
  ids with parent ids remapped; fork root gets `parentMessageId: null`;
  `branchIndex` flattened to `0`, `depth` recomputed `0..n` linearly,
  `isActive: true` throughout; `taskId: null` always; `metadata` sanitized
  by recursively dropping any key containing `"task"`; chat stats reset and
  recomputed. Integration-tested including rollback-on-mid-insert-failure
  and post-fork independence
  (`tests/integration/chat-fork.integration.test.ts`).
- **Task interplay**: branch switch and fork touch no task state — a
  streaming task keeps writing to its `assistantMessageId` even if the user
  switches branch mid-generation; the per-chat `ChatTaskLock` is keyed by
  `chatId`, not by branch. Archive refuses on the active branch; the archive
  loop is also non-transactional (deliberately not copied here).

## Decision

1. **The active path is a per-message flag, not a pointer or a path
   table.** `MessageRecord` gains `parentMessageId?`, `depth`, `branchIndex`,
   and `active` (`packages/host/src/ports/conversation-store.ts`). A chat is
   a forest; `active` marks which root-to-leaf path through it the
   conversation currently *is*. This is the representation copied from
   OneMind specifically because it survived production there — "give me the
   conversation" stays a single indexed read (`WHERE active ORDER BY
   depth`), never a walk, and a chat nobody has branched is the exact same
   query it always was (`branchIndex: 0` throughout, `active: true`
   throughout — the degenerate, byte-identical case).
2. **`orderKey` keeps its existing job, narrowed.** A child's `orderKey` is
   always greater than its parent's — a parent must exist before a child can
   name it — so along any root-to-leaf chain `depth` and `orderKey` agree,
   and `(depth, orderKey)` ordering equals plain `orderKey` ordering on the
   active path. That equivalence is why `listMessages`' existing
   `afterOrderKey` forward-paging cursor keeps working, unmodified, through
   a branched chat: the invariant makes the tree's path ordering and the
   flat counter's ordering the same order.
   - **Precisely: the cursor is a cursor into a PATH, not into a chat.**
     Paging forward through the path a cursor was taken on is unchanged in
     every respect. Paging across a *switch* is not, and cannot be:
     activating an older branch makes the live path one whose keys are all
     *below* a cursor taken on the branch just left, so the next page comes
     back empty — indistinguishable, to a naive client, from "no new
     messages". This is a property of a single monotonic counter over a
     tree, not a defect in the cursor, and the fix is on the reader: a
     client that can switch branches (its own switch, or another client's)
     must detect the path changed and re-list from the start rather than
     continue the cursor. Comparing the active leaf's id between reads is
     the cheap way to see it. Documented on
     `ListMessagesOptions.afterOrderKey`.
3. **Append-and-activate is one operation, not two.**
   `AppendMessageInput.parentMessageId` is the field that turns a linear
   append into a branch: absent, the parent is the chat's current active
   leaf (every existing caller, unchanged); present, it creates a new
   branch — `branchIndex` one past the highest used among that parent's
   children — **active immediately, with the whole path switched to it in
   the same write**. This is a deliberate improvement over OneMind's
   create-then-activate two-step: a window between "the branch exists" and
   "something is active" is a window in which "what is this conversation?"
   has no answer, and a crash inside it leaves a chat that reads as empty.
   AgentKit's version has no such window because there is no such step.
4. **`activatePath` is transactional and tested; OneMind's is neither.**
   Switching branches — making a message's path the active one — is exactly
   the case a "loop over individual updates" gets wrong under a crash: a
   half-applied switch is a chat with two active leaves, or none, and
   `listMessages` cannot report either as a conversation. Both reference
   adapters make the whole recompute-and-diff one transaction; the shared
   tree arithmetic (`activePathOf`, `activationSetOf`,
   `nextBranchIndex`, `packages/host/src/conversation/message-tree.ts`) is
   conformance-tested once, pure and synchronous, and called identically
   from both adapters — a `bun:sqlite` transaction cannot survive an
   `await`, so the walk has to be synchronous for a sqlite adapter to be
   able to use it inside one. It **returns the path it made live**, because
   the walk has already computed it: a caller that instead re-read the chat
   afterwards would be reading outside the transaction that wrote the flags,
   and could report a path a concurrent append had already moved on from.
   The `activateBranch` REST route answers with exactly that value.
   - **Descent semantics, precisely:** activating a message makes active
     its ancestors, itself, and a descent from it to a leaf that at each
     step prefers a child that is **already active**, falling back to the
     lowest `branchIndex` when none is. The preference only ever has
     something to prefer from when the node being activated is **already
     on the current active path** — e.g., re-activating an ancestor of
     where the conversation already is is a no-op walk down the same
     branch. The moment a switch moves the active path *away* from a
     branch, that whole abandoned subtree's `active` flags are cleared in
     the same transaction (both adapters recompute and write every
     record's flag from the freshly computed set, not just the divergence
     point). So switching back to that branch later does not "remember"
     which descendant was showing — there is nothing left marked active to
     prefer, and the descent falls back to the lowest `branchIndex`
     regardless of what was open before. "Feels like memory" therefore
     describes staying inside the current branch while renaming which
     ancestor is the activation target, not resuming an abandoned one.
5. **`forkChat` copies the active-path prefix, transactionally, stripping
   task linkage — OneMind's proven semantics, not the whole tree.**
   `ConversationStore.forkChat(chatId, fromMessageId)` requires
   `fromMessageId` to be on the source chat's active path (unknown, in
   another chat, or on an inactive branch all raise `InvalidForkPointError`
   — code `invalid_fork_point`, REST 400, added to the closed
   `HostErrorCode` union). The copy is flattened: fresh ids, parents
   remapped onto the copies, the first message a root, every `branchIndex`
   reset to `0`, `depth` recounted `0..n`, everything active — a fork is a
   straight line again, and branching it later starts a fresh tree owing
   the original nothing. Deliberately **not** carried over: `runId` on every
   copied message (the runs still belong to the source chat's task log; a
   copy pointing at them would make one run look like it wrote messages
   into two conversations), and any message with `metadata.placeholder:
   true` (a still-streaming answer is not history — the filter runs *after*
   the prefix cut, so forking from a message *while* a later answer is
   still streaming is legal and simply does not copy the in-flight part).
   Deliberately **kept**, against the instinct to drop it: `internal: true`
   records — the fork must still be able to replay tool calls and their
   results to the provider, and a copy missing them would show the model
   answers with no evidence behind them.
6. **Branch execution stays serialized per chat — `scopeId` is unchanged.**
   A message tree changes storage and context assembly, never task
   scoping: two branches of one chat still never generate concurrently this
   phase. This is deliberately the OneMind-proven semantics, not an
   oversight — switching the active branch **never cancels** a task already
   running against the branch a user switched away from; that task keeps
   writing its assistant message to completion, and the message may end up
   landing on a branch that is no longer active. A client watching the
   active path simply will not see it arrive unless it switches back.
7. **A run's records CHAIN off the run's own last write, never off "the
   active leaf" — `AppendMessageInput.activate: false`.** Decision 6 lets a
   turn outlive the branch it started on, and that is only coherent if
   *everything* the turn writes stays with it. A turn writes several
   records over the life of one run — the internal assistant turn carrying
   `tool_calls`, one tool result per call, host banners — and each was
   originally an unparented append, i.e. "hang this off whatever the active
   leaf is *at write time*". A branch switch between two of those writes
   therefore split one turn across two conversations: the later records
   surfaced on a branch that never ran them (visible in `listMessages` and
   over REST), while the run's own branch was left with an assistant turn
   whose `tool_calls` nobody answered — which the next replay of that
   branch has to reconcile as a synthetic failure for a tool that actually
   succeeded (`reconcileOrphanToolCalls`). Narrowing the window was not an
   option; the window is the whole run. So `appendMessage` gained
   `activate?: boolean` (default `true`, every existing caller unchanged),
   and `activate: false` + `parentMessageId` is a **chain append**:
   `branchIndex` assigned as usual, `active` **inherited from the parent**,
   and **no path switch**. `TurnRunner` tracks the id it wrote last —
   seeded with the placeholder from the task payload — and chains every
   projected append off it.
   - **Inheritance is a rule, not a copy of one flag**, so the invariant —
     one root-to-*childless*-leaf active chain — holds in all three cases:
     (a) parent is the current active leaf → `active: true`, and the new
     record is itself childless, so it is the same chain one message longer
     and byte-identically what the unparented append produced; (b) parent is
     inactive (the mid-run branch switch) → `active: false`, no flag
     anywhere changes; (c) parent is active but **already has an active
     child** — a bare `appendMessage` from another caller landed between two
     of the run's writes → `active: false`, because this chain was continued
     by somebody else and inheriting `true` would give one message two
     active children, which is not a path. Case (c) is the reason the rule
     is "active AND still the end of the live chain" rather than
     "`parent.active`"; the sqlite adapter answers it in the same indexed
     query that already computes the next `branchIndex`, so it costs no
     extra read.
   - **`activate: false` without `parentMessageId` is rejected**
     (`invalid_append`), not reinterpreted. The only parent an unparented
     append could take is the active leaf, which is precisely the race the
     flag exists to remove, so a caller meaning "continue the conversation"
     must activate and one meaning "continue *my* chain" must name the
     link.
   - Graded by the conformance suite on both adapters (all three inheritance
     cases, no-switch, and the rejection), plus a seeded random
     walk that composes chain appends with branch appends and switches and
     re-checks the whole shape invariant after every step
     (`packages/testing/src/conversation-tree-driver.ts`).

## Alternatives considered

- **A pointer to the active leaf, or a separate path table, instead of a
  per-message flag.** Rejected: both need either a walk up from the pointer
  or a join against the path table to answer "what is this conversation" —
  the per-message flag is what makes it a single indexed read, and it is
  the representation that has actually survived production in OneMind.
- **Archiving or deleting a branch.** Deferred, not designed: nothing in
  this phase needed it, and OneMind's own archive loop is non-transactional
  in a way this ADR does not want to import silently. Left for a future
  phase (see `docs/roadmap.md`).
- **Cancel the running task when a user switches away from its branch.**
  Rejected: branch execution is already serialized per chat (`scopeId`
  unchanged), which already bounds how much concurrent generation a switch
  could even be racing against, and OneMind's proven behavior is to let the
  task finish and simply not surface it on an inactive path. Cancelling
  would throw away real provider work and tokens already spent for a UX
  problem the serialization already limits the blast radius of.

## Consequences

- A chat nobody has branched behaves byte-identically to before this ADR:
  `listMessages`, `afterOrderKey` paging, and `submitMessage` without
  `parentMessageId` are unchanged in shape and behavior.
- **A fork's STORED order is provider order — the repair happens once, at
  copy time.** `orderMessagesForProvider`
  (`packages/host/src/turn/message-order.ts`) applies its provider-legal
  reordering (internal assistant turn → tool results → visible answer) only
  to records that share a `runId`, and a fork strips `runId` from every
  copy — so a forked chat has *nothing left to repair itself with*. The
  source order it would otherwise inherit is **not** already correct, which
  an earlier draft of this ADR wrongly asserted: the visible answer is
  created FIRST, as an empty placeholder at submit time, so it carries a
  *lower* `orderKey` than the internal assistant turn and the tool results
  that produced it. A verbatim copy therefore replays the answer before the
  tool call that produced it — permanently, since the fork can never
  re-derive the grouping. `forkPrefixOf` (`message-tree.ts`) consequently
  runs the prefix through `orderMessagesForProvider` **before** flattening,
  so `depth`, `orderKey` and the parent chain are all assigned on the
  repaired sequence and the copy is provider-legal by construction. Graded
  in `fork-conformance.ts` on both adapters.
- The sqlite reference adapter moves to `SCHEMA_V4`
  (`parent_message_id`, `depth`, `branch_index`, `active` on `messages`,
  plus the two indexes the active-path and sibling reads want); no
  migration ships (workspace-private, `PRAGMA user_version` fail-closed —
  see `internal/reference-adapters/README.md`'s SQLite schema section).
- `docs/roadmap.md`'s P5a entry moves to Done, referencing this ADR.

## Out of scope (deliberate)

Branch/chat archiving and deletion; concurrent generation across branches of
one chat (`scopeId` stays chat-scoped); a cursor that can page across a branch
switch (see decision 2 — clients re-list instead); message search and
forward-paging past the active path (P5b); attachments (P5c).
