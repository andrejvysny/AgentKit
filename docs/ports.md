# Port catalog

`@agentkit/host` defines no storage or execution of its own — it defines
interfaces ("ports") an embedding host implements, plus the record types and
frozen transition tables those interfaces are defined in terms of. Two
complete implementations ship as their own packages:
[`@agentkit/adapters-memory`](../packages/adapters-memory)'s
`MemoryAssistantStore` (Map-backed, for tests and local dev) and
[`@agentkit/adapters-sqlite`](../packages/adapters-sqlite)'s
`SqliteAssistantStore` (`bun:sqlite`-backed, the durable store for a
single-process host). Both pass
`@agentkit/testing`'s `describeAssistantStoreConformance(factory)` suite
([`packages/testing/src/store-conformance.ts`](../packages/testing/src/store-conformance.ts)),
the shared behavioral contract every `AssistantStore` implementation must
satisfy; a new adapter is graded against the same suite rather than by what
one host's tests happen to touch.

Source: [`packages/host/src/ports/`](../packages/host/src/ports/).

## Persistence

### `AssistantStore` (aggregate)

[`ports/assistant-store.ts`](../packages/host/src/ports/assistant-store.ts)

The host's persistence, as one aggregate of six stores
(`conversations`, `tasks`, `proposals`, `providers`, `settings`, `outbox`)
plus `transaction<T>(fn)`. Grouped rather than injected separately because
the operations that matter span them — submitting a turn writes a user
message, a placeholder assistant message, and a task row together; finishing
one writes events, a message, and a status together — and those writes must
land as one unit or not at all.

**Key invariant**: `transaction(fn)` either commits every write `fn` makes
or rolls all of it back on a throw. An adapter that cannot roll back (a
plain in-memory store) must declare `capabilities.atomicTransactions: false`
to the conformance harness rather than silently pass a weaker guarantee.

**Reference / conformance**: `MemoryAssistantStore` in
[`packages/adapters-memory/src/`](../packages/adapters-memory/src/) and
`SqliteAssistantStore` in
[`packages/adapters-sqlite/src/`](../packages/adapters-sqlite/src/);
conformance suite in
[`packages/testing/src/store-conformance.ts`](../packages/testing/src/store-conformance.ts).

### `ConversationStore`

[`ports/conversation-store.ts`](../packages/host/src/ports/conversation-store.ts)

Chats and messages. Owns `MessageRecord.orderKey`, the per-chat ordering
key — not `createdAt`, for the same reason `AiRunEvent.seq` orders events:
several messages can be written in one transaction within the same
millisecond.

**`MessageRecord.content` is `string | AiContentPart[]`** (`AiMessageContent`,
`packages/contracts/src/content.ts`), and so are `AppendMessageInput.content`
and `UpdateMessagePatch.content`. A store round-trips parts **losslessly** and
inspects nothing inside them — in particular an image part may name a host
attachment (`source: { kind: "ref", ref }`) rather than carry its bytes, and
the ref is what is persisted. `TurnRunner` resolves refs per provider pass
without ever rewriting the record (see [`AttachmentResolver`](#attachmentresolver)); a store
that "helpfully" inlined the bytes would make every fork and every page of the
conversation carry them. `role: "tool"` and `role: "system"` records are
strings by construction.

**A chat is a tree; the active path is a per-message flag** ([ADR
0007](adr/0007-conversation-branching-fork.md)). `parentMessageId` makes a
chat's messages a forest (one root normally); `active` marks which
root-to-leaf path through it the conversation currently *is* — storing the
path as a flag on every message, rather than a pointer to a leaf or a path
table, is what makes reading "the conversation" a single indexed read
instead of a walk. A child's `orderKey` is always greater than its
parent's, so `depth` and `orderKey` agree along any root-to-leaf chain —
which is why `(depth, orderKey)` ordering and plain `orderKey` ordering are
the same order on the active path, and `listMessages`' `afterOrderKey`
cursor keeps paging correctly **within one path**. It is a cursor into a
path, not into a chat: after a switch to an older branch, every key on the
live path is *below* a cursor taken on the branch just left, so the next
page reads empty rather than reporting a different conversation. A client
that can switch branches must notice the path changed — comparing the
active leaf's id between reads is the cheap way — and re-list instead of
continuing the cursor. A chat nobody has branched is the degenerate case:
`branchIndex: 0` and `active: true` throughout, `listMessages`
byte-identical to before branching existed.

**Tree operations**, all invariant-preserving and never left to a caller:

- `appendMessage`'s `parentMessageId` (optional) is what turns a linear
  append into a branch: absent, the parent is the chat's active leaf
  (every pre-branching caller, unchanged); present, it creates a new
  branch (`branchIndex` one past the highest used among its siblings) that
  is **active immediately, with the whole path switched to it in the same
  write** — append-and-activate is one atomic operation, not two, so there
  is no window in which the branch exists but nothing is active.
- `appendMessage`'s `activate` (optional, default `true`) suppresses that
  switch. `activate: false` **requires** `parentMessageId` and is a **chain
  append**: `branchIndex` assigned as usual, `active` *inherited from the
  parent*, no path switch. It exists for a run writing the records of the
  turn it is already executing (`TurnRunner` chains every internal
  assistant record, tool result and banner off the id it wrote last, seeded
  with the placeholder), so a user switching branches mid-run cannot pull
  the second half of that turn onto a conversation that never ran it — nor
  strand the run's own branch with tool calls nobody answered. The inherited
  flag is a rule, not a copy: `active` is true only when the parent is active
  *and still the end of the live chain*. Chaining off the active leaf extends
  that chain; chaining off an inactive node touches no flag anywhere; chaining
  off an active parent that something else has already continued goes off-path,
  because one message with two active children is not a path. `activate: false`
  with no parent is `invalid_append`. See ADR 0007 decision 7.
- `listSiblings(messageId)` — the messages sharing a message's parent,
  itself included, `branchIndex` ascending (a root's siblings are the
  chat's roots).
- `activatePath(messageId)` — makes a message's path the chat's active one,
  atomically, and **returns that path** (what `listMessages` would answer
  next): its ancestors, itself, and a descent to a leaf that prefers an
  already-active child at each step, falling back to the lowest
  `branchIndex` when none is. That preference only has something to prefer
  when the node being activated is already on the current active path —
  switching *away* from a branch clears every descendant's `active` flag in
  that same transaction, so switching back to it later does not "remember"
  which child was showing; it falls back to the lowest `branchIndex`. The
  path is returned rather than re-read because the switch already computed
  it, and a read-back would happen *outside* the transaction that wrote the
  flags.
- `forkChat(chatId, fromMessageId)` — copies the active path up to and
  including `fromMessageId` into a **new** chat, in one transaction.
  `fromMessageId` must be on the source chat's active path or this raises
  `InvalidForkPointError` (`invalid_fork_point`). The copy is flattened
  (fresh ids, `branchIndex` reset to `0`, `depth` recounted `0..n`,
  everything active) and strips `runId` and any `metadata.placeholder:
  true` message, while keeping `internal: true` records so the fork can
  still replay tool calls to the provider. The prefix is run through
  `orderMessagesForProvider` **before** it is flattened, so the copy's
  stored order already *is* provider order — stripping `runId` leaves the
  fork with no way to repair that ordering later, and a source chat writes
  its visible answer (as an empty placeholder) *before* the internal turn
  and tool results that produced it. See ADR 0007 for the full rationale.

**Lifecycle and retrieval**:

- `ChatRecord.archived` (a real field, not a `metadata` key, so a store can
  index it). `false` on every created chat and on every fork — a copy of an
  archived chat is a new conversation somebody just made.
- `updateChat(chatId, { title?, metadata?, archived? })` returns the updated
  record; unknown chat → `RecordNotFoundError`. `metadata` **replaces** (same
  rule and rationale as `updateMessage`), and every write bumps `updatedAt`,
  which is `listChats`' sort key.
- `ListChatsOptions.includeArchived` (default `false`) excludes archived chats
  from the listing; `ListChatsOptions.ids` is an exact-id **batch fetch** that
  still sorts and pages like the listing and **resolves archived chats
  regardless** — archiving hides a chat from browsing, not from a caller who can
  already name it. An empty `ids` array returns nothing.
- `ListMessagesOptions.beforeOrderKey` pages **backwards**: the `limit` active-path
  messages strictly below the key, still ascending. Setting it together with
  `afterOrderKey` is `invalid_cursor` — a range read is a third operation with
  its own undefined answer to "which end does `limit` count from".
- `deleteChat(chatId)` removes the chat and **every** message in it, off-path
  branches included, transactionally; unknown chat is `not_found`, never a
  silent success. It is scoped to the conversation on purpose: tasks and
  proposals live in other stores, and deciding what a delete *means* is
  `ConversationService.deleteChat`'s job (below).
- `importConversation(input)` — the **history-migration primitive**: writes a
  whole conversation in one transaction, **preserving the caller's ids**. The
  caller owns identity, the parent links and the `active` flags; the STORE
  assigns `orderKey` (creation order), `depth` and `branchIndex` by the same
  sibling rules every append follows. Messages must be in creation order.
  Validated in full before any write, all-or-nothing, each failure an
  `InvalidImportError` (`invalid_import`) whose `details.reason` is one of
  `duplicate_chat`, `duplicate_message_id`, `unknown_parent`, `forward_parent`,
  `no_active_path`, `broken_active_chain`, `active_leaf_has_child`. An empty
  message list is a valid empty chat; a non-empty list with nothing active is
  not. The validation is the shared, pure `planImportedMessages`
  (`conversation/message-tree.ts`), so both adapters accept exactly the same
  payloads.
- `searchMessages(query, { chatId?, limit? })` → `MessageSearchHit[]`
  (`{ chatId, messageId, snippet }`), best match first. **Optional** on the port
  and graded behind the conformance harness's `capabilities.search`. The
  searched text of a message is its string body or **all** of its text parts
  joined with `"\n"` (`searchTextOf` — indexing only the first part is a real,
  silent bug this shared projection exists to prevent). `internal: true` and
  `placeholder: true` records are excluded, and both flags are read at **query**
  time because metadata is rewritten after the fact. Hits are **not** restricted
  to the active path: a message on an abandoned branch is still something that
  was said in this chat, and is exactly what "where did I see that?" is looking
  for. A query that sanitizes to nothing returns `[]` rather than raising.
  Snippets mark matches with the shared `SEARCH_MATCH_START`/`SEARCH_MATCH_END`
  and elide with `SEARCH_SNIPPET_ELLIPSIS`.

**Key invariants**: `appendMessage` assigns the next `orderKey` in the store,
not the caller, so concurrent appends from a run and from a user land in a
defined order. `updateMessage`'s `metadata` patch *replaces* the stored bag
rather than merging — a merge would make "unset this flag" unexpressible;
it never touches a message's position in the tree
(`parentMessageId`/`depth`/`branchIndex`/`active` are immutable once
written — branching creates a new message instead of moving one).
`activatePath` and `forkChat` are both transactional — a half-applied
branch switch or a partially-copied fork is not an allowed intermediate
state a reader can observe.

**Key invariant, tree side**: the active messages of a chat form exactly one
chain from a root to a *childless* leaf, after every operation and every
sequence of them. Graded twice: by the named branching tests, and by a seeded
random walk (`packages/testing/src/conversation-tree-driver.ts`) that composes
appends, branch appends, chain appends and switches at random and re-checks the
whole shape — links, depths, rising `orderKey`, childless leaf — after every
single step, on both reference adapters.

**Chat lifecycle across stores**: `ConversationService`
([`conversation/conversation-service.ts`](../packages/host/src/conversation/conversation-service.ts))
is where a delete that *means something* lives. `deleteChat(chatId)` runs one
`store.transaction`: read the chat, read `TaskStore.listByScope(chatId)`, refuse
with `ChatBusyError` (`chat_busy`, HTTP 409) if any task is `running` or
`waiting_approval`, then `conversations.deleteChat` + `tasks.deleteByScope` +
`proposals.deleteByChat`. The check and the deletes share one transaction
because a task read outside it could go `running` in between, which would make
the refusal a race. `queued` is deliberately not a busy status — nothing has
been spent on it, and refusing would make a chat undeletable for as long as
anything sat behind it. Force-cancelling live runs is a *different* operation
with different consequences, so it is the caller's explicit call, never a side
effect of a delete. The scope is the chat id, which is the convention
`TurnRunner` writes with. `archiveChat`/`unarchiveChat` are thin, deliberately
logic-free wrappers over `updateChat({ archived })`.

**Reference / conformance**: `MemoryAssistantStore` and `SqliteAssistantStore`
(sqlite adapter: `SCHEMA_V6`, `parent_message_id`/`depth`/`branch_index`/
`active` on `messages`, `chats.archived`, `settings.tool_calling_mode`, and
the FTS5 view/table/trigger set behind `searchMessages` — see
[`packages/adapters-sqlite/README.md`](../packages/adapters-sqlite/README.md#schema-v6)),
both graded by the same conformance suite; the tree arithmetic itself
(`activePathOf`, `activationSetOf`, `nextBranchIndex`, `forkPrefixOf`,
`planForkedMessages`) is shared, pure, and synchronous
(`packages/host/src/conversation/message-tree.ts`), so both adapters answer
"what does a branch switch activate" and "what does a fork copy" identically
by construction rather than by two hand-written walks agreeing.

### `TaskStore`

[`ports/task-store.ts`](../packages/host/src/ports/task-store.ts)

Durable lifecycle for a task of **any** kind: status (`TaskStatus`,
`TASK_TRANSITIONS`), attempts, leases, and the event log — the product every
UI replays and every crash recovery reads. `TaskRecord { taskId, kind,
scopeId, status, priority, enqueuedAt, availableAt, payload, attemptCount,
poisonCount, parentTaskId?, dependsOn?, progress?, … }` is kind-agnostic:
`kind` is an opaque string the store
filters and returns (see [`docs/architecture.md`](architecture.md#task-kinds-and-executors)
for what dispatches on it), and everything specific to a kind — a chat
turn's `chatId` included — rides in `payload`, not on the record itself.
`parentTaskId` (lineage, set only via `TaskExecutionContext.spawnChild`) and
`dependsOn` (the claim gate) are two distinct edges — see [Task dependencies
and subagents](architecture.md#task-dependencies-and-subagents) and [ADR
0003](adr/0003-task-dependencies-and-subagents.md). `progress` is a mutable,
overwritten snapshot, never an event — and it belongs to the TASK, not to an
attempt: it survives a failed attempt and a retry, so attempt 2 reads attempt
1's snapshot until it writes its own.

**Key invariants**:
- `createTask` MUST reject a `taskId` that already exists with
  `DuplicateTaskError`, never silently overwrite — the id is the caller's
  idempotency key (a retried submit, a redelivered message), and
  overwriting would discard a live task's payload and attempt history while
  its event log stayed behind. It MUST also reject an unknown `parentTaskId`
  or `dependsOn` entry (or `dependsOn` naming the task's own id) with
  `UnknownDependencyError` — every dependency must already exist when the
  dependent is written, which is what makes the dependency graph a DAG by
  construction rather than by runtime cycle detection.
- `transitionTask` is compare-and-set: it MUST reject with
  `InvalidTaskTransitionError` when the task's current status is not in the
  caller's `from` set (someone else moved it first — a lost race, not a
  retryable hiccup), and MUST reject a transition not in
  `TASK_TRANSITIONS`. `TASK_TRANSITIONS` admits one `queued → failed` edge,
  reserved for the dependency cascade below.
- `appendEvents`/`listEvents` are typed on `TaskEventEnvelope`
  (`@agentkit/contracts`) — the kind-agnostic shape the store actually
  orders (`seq`) and dedups (`eventId`); `AiRunEvent` is the `chat.turn`
  vocabulary and structurally satisfies it. `appendEvents` MUST reject a
  stale `leaseToken` (`LeaseLostError`) and a non-monotonic `seq`
  (`SeqConflictError`); it MUST NOT re-stamp `seq` — the emitter owns
  numbering (core's stamper inside a chat pass, `createTaskEventWriter`
  elsewhere).
- `claimNext` MUST be atomic: claiming a task creates its attempt and lease
  in the same operation, so no other caller can claim the same task.
  `ClaimNextInput.kinds?` optionally restricts the claim to a set of kinds,
  for a deployment whose worker pools register different executor sets —
  absent means "any kind". **Dependencies are enforced here, not by a
  separate reaper**: a task with an unfinished `dependsOn` entry is skipped;
  a task whose dependency ended badly is settled INSTEAD of claimed —
  `evaluateTaskDependencies` (exported beside `assertTaskTransition`, for
  the same "every adapter reaches the same verdict" reason) reduces
  dependency state to `ready`/`blocked`/`settle`, and a `settle` verdict
  transitions the dependent `failed` (`dependency_failed: <id>`, via the
  `queued → failed` edge) or `cancelled`, lazily, on the claim path. Lazily
  means lazily: the candidate filter (`queued`, `availableAt`, `scopesBusy`,
  `kinds`) runs BEFORE the verdict, so a doomed dependent whose scope has a
  task running — or whose `kind` this worker does not claim — is not a
  candidate and stays `queued` until a claim could actually have taken it.
  Concurrent claims may also race for the same candidate; the loser of the
  compare-and-set skips it and keeps walking, so a lost CAS is not an error
  the caller sees.
- `listChildren(taskId)` returns tasks whose `parentTaskId` is `taskId`, one
  level (not the subtree) — the one caller that needs the subtree
  (`TaskService.cancelTask`) already walks breadth-first and asks again.
- `listByScope(scopeId)` returns every task in a scope, any status, unpaged —
  deliberately the narrowest query that answers "is anything still live here?",
  which is the only question a caller outside this port has
  (`ConversationService.deleteChat` asks it). A general `listTasks(filter)`
  would owe every adapter statuses, kinds, ordering and paging to serve one
  consumer.
- `deleteByScope(scopeId)` deletes a scope's tasks with their attempts, leases
  and event log, and returns the number of **tasks** removed. Unconditional by
  design: whether live work may be discarded is policy, and this port cannot
  answer it — the caller decides (and `ConversationService.deleteChat` refuses
  outright while anything is `running` or `waiting_approval`).
- `updateProgress(taskId, progress, opts)` REPLACES `TaskRecord.progress`
  wholesale (never merges) and MUST reject a stale `leaseToken` with
  `LeaseLostError`, the same check `appendEvents` makes. It never touches
  the event log, and nothing clears it between attempts — a retry starts
  with the previous attempt's snapshot still in place until it writes one.

**`waiting_approval` is currently producer-less.** No code in this repository
moves a task into it: a staged write returns `pending` to the model and the
task completes normally (see the proposal lifecycle below). The status and
its transitions are reserved for hosts that instead park a task on approval
and resume it when the decision arrives — `TASK_TRANSITIONS` already admits
`running → waiting_approval → running | completed | failed | cancelled`.

**Reference / conformance**: `MemoryTaskStore` / `SqliteTaskStore`. Two
different guards, for two different questions (the SQL shapes below are
`SqliteTaskStore`'s; `MemoryTaskStore` does the Map equivalent):

- **Fencing** (`appendEvents`, `renewLease`) SELECTs the task's current
  lease inside the store transaction and compares tokens in code, rejecting
  a stale one with `LeaseLostError`. There is no `WHERE lease_token = ?` on
  the write: a no-op UPDATE cannot say whether the lease moved or the row
  never existed, and the two are different errors.
- **Guarded write + `changes` count** is used where losing a race IS the
  answer: `releaseLease` deletes `WHERE lease_token = ?` and reads
  `changes === 0` as "not current"; `transitionTask`'s status CAS updates
  `WHERE task_id = ? AND status = ?` and reads `changes === 0` as "changed
  underneath the SELECT" (`InvalidTaskTransitionError`).

The conformance suite covers CAS rejection, lease renewal/expiry with a
strictly higher `fencingToken` on re-acquire, `seq` monotonicity rejection,
atomic `claimNext` under a busy scope, kind round-trip, duplicate-task
rejection, and the `kinds` claim filter — the last of these arranged so that
only a real filter passes it (the wanted kind is the one the priority and
FIFO ordering would NOT have picked), plus `kinds: []` meaning "no kind is
acceptable" rather than "any kind". It also covers dependency gating and the
failure/cancel settle verdicts, `listChildren`, `updateProgress`'s
lease-gating, and (opt-in, see `packages/host/src/ports/task-aging.ts`)
priority aging — a new adapter is graded against the same suite.

**Concurrent-durability invariants**, a second and stronger bar beside the
conformance suite ([ADR 0006](adr/0006-hardening-tranche.md)):
`@agentkit/testing`'s `checkTaskInvariants`/`snapshotTaskInvariants` state
what must hold of a `TaskStore` regardless of what concurrent activity got
it there (never two live claims for one task, fencing strictly monotonic,
`seq` gapless), graded against a seeded, replayable random schedule
(`runTaskSchedule`) rather than one hand-written scenario. Both reference
adapters also support and are tested against **multiple store handles over
one backing file/store** — two `SqliteAssistantStore` instances on one
sqlite file, or two `MemoryTaskStore`s over the same in-memory-equivalent
topology — a real deployment shape a single per-instance claim mutex cannot
cover; SQLite's own transactionality (a busy-wait strategy tuned separately
for synchronous vs. `await`-holding transactions) does the work instead.
One gap remains, documented rather than fixed: a synchronous transaction on
one handle cannot event-loop-wait for another handle's in-flight
*asynchronous* claim in the same process (see ADR 0006's Consequences and
`docs/roadmap.md`'s Later list).

### `ProposalStore`

[`ports/proposal-store.ts`](../packages/host/src/ports/proposal-store.ts)

Staged writes: `ProposalRecord`, `ProposalStatus`, and apply outcomes
(`ApplyOutcome`, keyed by `operationId`).

**Key invariants**:
- `create` MUST enforce `UNIQUE(scopeKey, actionId)` when `actionId` is set,
  except among proposals in `ACTION_ID_RELEASING_STATUSES`
  (`rejected` | `invalidated`) — those never wrote anything, so their key is
  free to reuse. Holding the reservation on those statuses would strand a
  model told to derive a stable key from intent.
- `getByActionId` returns the **most recent** proposal for a
  `(scopeKey, actionId)` pair — recency, not uniqueness, decides which
  record answers a dedup check, since a released key can be reused.
- `recordOutcome` is idempotent per `operationId`: a second call with the
  same id must return the *first* outcome, never overwrite it.
- `deleteByChat(chatId)` deletes a chat's proposals **and the apply outcomes
  they claimed**, returning the number of proposals removed. **By `chatId`, not
  by `scopeKey`** — a proposal carries both, and they are not the same set: two
  chats routinely stage writes into one shared document, so a scope-keyed delete
  would silently take a bystander's staged writes. Outcomes go with their
  proposals because an `ApplyOutcome` is keyed by an `operationId` only the
  proposal row still names; left behind it is unreadable and indistinguishable
  from a leak. Unconditional, for the same reason `deleteByScope` is.

**Reference / conformance**: `MemoryProposalStore` / SQLite equivalent
(`proposals` + `proposal_outcomes` tables, a partial unique index on
`(scope_key, action_id) WHERE action_id IS NOT NULL AND status NOT IN
('rejected','invalidated')`); conformance suite covers the duplicate-key
rejection, key reuse after release, and outcome idempotency.

### `ProviderStore`, `SettingsStore`, `OutboxStore`

[`ports/provider-store.ts`](../packages/host/src/ports/provider-store.ts),
[`ports/settings-store.ts`](../packages/host/src/ports/settings-store.ts),
[`ports/outbox-store.ts`](../packages/host/src/ports/outbox-store.ts)

`ProviderStore` persists configured providers, their model catalogs, and
probed capabilities (`replaceModels` is a wholesale replace, not a merge —
a refresh is a snapshot). `SettingsStore` is one row of assistant-wide
settings; `toolCalling` (`"auto" | "on" | "off"`, default `"auto"`) is the
manual override on top of `ProviderStore`'s **probed** `toolCalling`
capability — `on` stages tools even when the probe said unsupported (probing
is a heuristic against someone else's server, and a wrong `false` would
otherwise leave a capable model permanently toolless), `off` stages none at
all and never calls the contributors. `OutboxStore` is the transactional-outbox pattern: a run's events
are written to the run log in the same transaction as the state they
describe, and publishing them outward (SSE, a websocket, a message bus) is
a separate, retryable step keyed on `claimBatch`/`markPublished`/
`markFailed` — without it, a host would have to choose between announcing
work that may still roll back and losing the announcement on a crash.

**Key invariant** (`OutboxStore`): `claimBatch` must not hand an in-flight
record to a second claimer before it is resolved — the reference adapter
does this by pushing `availableAt` forward on claim, the same trick a
visibility-timeout queue uses.

## Execution

### `TaskRunner` / `TaskWorker`

[`ports/task-runner.ts`](../packages/host/src/ports/task-runner.ts)

The durable queue that turns "a task exists" into "a worker is executing
it". Deliberately has **no `subscribe()`** — events reach consumers through
the task event log and the outbox, both of which survive a restart; a
subscription on the runner would be a second, lossier channel.

**Key invariants**:
- `enqueue` is idempotent per `taskId`: a redelivered enqueue for a task
  that is no longer `queued` is a silent no-op, never a second execution of
  one task.
- `recover()` is the startup pass: expire dead leases, end their attempts
  `abandoned`, reconcile interrupted applies by operation id, then
  re-enqueue or dead-letter — run before any worker starts claiming.

`StartWorkerOptions` has no `kinds` field — the reference runner claims
every kind and relies on every executor a process needs being registered in
that one process's `ExecutorRegistry` (see
[`docs/architecture.md`](architecture.md#task-kinds-and-executors)); an
unregistered kind then fails with `ExecutorNotFoundError` rather than
waiting for a worker that could have run it.
`ExecutorRegistry.kinds()` is what a kind-filtering `TaskRunner` adapter
would feed to `ClaimNextInput.kinds` so a box only claims work it can run —
no adapter in this repository does that filtering yet.

`recoverOnBoot({ taskRunner, proposals })`
([`packages/host/src/bootstrap.ts`](../packages/host/src/bootstrap.ts))
is the pair of them a host actually calls at startup: `TaskRunner.recover()`
first, then `ProposalService.reconcileInterrupted()`, so a recovered task is
picked back up only after the writes it may have left mid-apply are settled.
Both halves are idempotent; it returns `{ proposalsReconciled }`. `recover()`
therefore runs BEFORE `startWorker`, and an implementation has to survive
that: its expiry pass deletes the very lease a later pass would have found the
task by, so a task it cannot hand to a worker yet must be remembered and
re-dispatched when one starts, not dropped.

**Reference / conformance**: `SingleProcessTaskRunner`
([`packages/runner-local/src/single-process-task-runner.ts`](../packages/runner-local/src/single-process-task-runner.ts)) —
claim/execute/heartbeat/retry/dead-letter/recover for one process, with
fire-and-forget dispatch (never awaits an execution inside its claim loop),
evidence-based error classification (`error-classifier.ts`: an unrecognized
failure is terminal by default, not blindly retried), and an exponential,
jittered delay between attempts of one task. Explicitly single-process:
cancellation of a task another process owns is not delivered — see
[`docs/non-goals.md`](non-goals.md). The port's own behavioral contract is
`@agentkit/testing`'s `describeTaskRunnerConformance(options)`
([`packages/testing/src/task-runner-conformance.ts`](../packages/testing/src/task-runner-conformance.ts)):
enqueue idempotency, recovery from an expired lease, lease renewal across an
attempt that outlives the TTL, cancellation reaching a running worker, and the
concurrency budget — run against both reference stores. The renewal scenario
asks `create()` for a short `heartbeatMs`; every other one needs renewal
effectively off, so an adapter's default must be longer than any test's real
lifetime.

### Task execution

[`tasks/task-executor.ts`](../packages/host/src/tasks/task-executor.ts),
[`tasks/executor-registry.ts`](../packages/host/src/tasks/executor-registry.ts),
[`tasks/task-service.ts`](../packages/host/src/tasks/task-service.ts),
[`tasks/task-event-writer.ts`](../packages/host/src/tasks/task-event-writer.ts)

The kind-dispatch layer built on top of `TaskStore`/`TaskRunner`: what turns
a claimed task into a running executor and a submitted request into a
persisted, dispatched task. Full description in
[`docs/architecture.md`](architecture.md#task-kinds-and-executors); the
invariants worth restating here because they are easy to violate by
accident:

**Key invariants**:
- `createDispatchingWorker` hands each `TaskExecutor` the already-loaded,
  already-guarded `TaskRecord` (`TaskExecutionContext.task`) — an executor
  never re-fetches it, so there is exactly one answer in play to "is this
  task still executable?".
- `TaskService.dispatch` (and therefore `submitTask`) MUST run strictly
  after the transaction that created the task has committed. Enqueuing from
  inside the transaction callback risks the claim loop claiming a row that
  a rollback then deletes out from under a running worker — the
  `bun:sqlite` join-transaction hazard noted on
  [`AssistantStore.transaction`](#assistantstore-aggregate).
- `createTaskEventWriter` is barred from use inside a chat pass. Inside a
  pass, core's `createEventStamper` owns `seq` numbering in memory; a
  second writer numbering from `TaskStore.nextSeq` against the same log
  would interleave two counters into one stream.
- `TaskExecutionContext.spawnChild` is present only when
  `createDispatchingWorker` was given a `TaskService`, and always presets
  `parentTaskId` to the executing task — an executor can fan work out but
  cannot forge or omit its own lineage.
- `TaskService.cancelTask` cascades breadth-first over `parentTaskId`
  lineage, never `dependsOn`; a running descendant is asked to stop
  cooperatively (`taskRunner.requestCancel`), never forced terminal. Full
  description: [Task dependencies and
  subagents](architecture.md#task-dependencies-and-subagents), [ADR
  0003](adr/0003-task-dependencies-and-subagents.md).

## Policy

### `WritePolicy`

[`ports/write-policy.ts`](../packages/host/src/ports/write-policy.ts)

Decides whether a staged write applies immediately or waits for a human.
Three modes: `auto_readonly_confirm_writes` (default — writes stage and
wait unless a standing allowance covers them), `confirm_all_writes` (no
allowance is ever honored), `auto_all` (trusted, fully-undoable hosts only).
`isAutoApplyAllowed` is **synchronous by design** — it is consulted inside a
write tool's execution, on the hot path of a model turn, and an IO-bound
answer could time out, turning "needs confirmation" into "tool failed".

**Key invariant**: allowances are risk-ranked (`RISK_RANK`: `low` < `medium`
< `high` < `destructive`); an allowance at rank N covers every proposal at
rank ≤ N, never higher — a grant for low-risk edits does not imply consent
to a destructive one, and a model cannot escalate by re-labeling its own
proposal's risk.

**Reference**: `SessionWritePolicy`
([`packages/host/src/policy/session-write-policy.ts`](../packages/host/src/policy/session-write-policy.ts))
— allowances live only for the process lifetime, on purpose: persisting a
"yes, go ahead" from a conversation the user was watching into a future
session they are not would silently extend consent past where it was given.

### `ProposalApplier`

[`ports/proposal-applier.ts`](../packages/host/src/ports/proposal-applier.ts)

The host side of a write — the only component that actually changes the
world. Everything else in the proposal pipeline is bookkeeping around this
call.

**Key invariant**: `getOutcome(operationId)` MUST answer for work that
already happened, even across a process restart — this is what
`ProposalService.reconcileInterrupted` calls to resolve a proposal a crash
left in `applying`, and there is no other way to ask "did my write land?"
without either losing a write or duplicating one. An apply that partially
lands MUST report `status: "partial"` with the failed operations, rather
than discarding the half that worked.

## Verification

### `VerificationHook`

[`ports/verification.ts`](../packages/host/src/ports/verification.ts)

Post-run "did the work actually land?" check — a run that called tools and
narrated success can still have achieved nothing. `verify()` returns a
`DeficiencyReport` (`status: "pass" | "partial"` plus `deficiencies`) or
`null` when the hook has nothing to say. Domain checks are entirely the
host's business; none are shipped by this framework.

**Key invariant**: `TurnRunner` calls this **once**, only when the run
actually made tool calls, and never feeds the deficiencies back for a
correction pass — see the multi-pass note in
[`docs/non-goals.md`](non-goals.md).

## Context and tools

### `ContextProvider`

[`ports/context-provider.ts`](../packages/host/src/ports/context-provider.ts)

What the host pins into a chat's context: bindings (the objects the model
is working on) and system-prompt text. Resolved **per run**, not stored on
the run, because the world moves between turns — a bound document can be
deleted or go stale between one turn and the next.

### `AttachmentResolver`

[`ports/attachment-resolver.ts`](../packages/host/src/ports/attachment-resolver.ts)

`resolve(ref) → { mediaType, base64 } | null`. Turns the `ref` image sources
in stored messages into bytes a provider can be shown. **The blob storage is
the host's** — a file, a row, an S3 key, a content-addressed cache — and the
ref is opaque: AgentKit never parses one, derives a path from one, or mints
one.

`null` is a normal answer, not an error path: an attachment can be deleted,
expired, or belong to a workspace the caller lost access to, and every one of
those is a conversation that must still run. Throw only for a genuine fault
(storage down), where failing the turn is honest.

**Resolution is in-memory and per pass.** `TurnRunner` resolves after
`assembleMessages` and before `runChat`, for every pass including retries; the
stored message always keeps the ref, so a later turn re-resolves it at
whatever fidelity — or refusal — applies then. Within one pass a ref resolves
once (cached); across passes, never.

**Budgets** (`TurnRunnerDeps.attachmentBudgets`, defaults 5 MiB per image /
20 MiB total / 16 images, borrowed from OpenPCB's `MENTION_LIMITS`) cap what
this port may add to a pass. An image that cannot be sent — unresolvable, or
over a cap — is dropped from what the provider sees, with one durable
`run.warning` naming the ref and the reason (`attachment_unresolved`,
`attachment_budget_exceeded`; see
[`docs/contracts.md`](contracts.md#warning-codes)). Degrade, never fail a turn
over an attachment. The budgets bound this port's contribution only: base64 a
caller inlined itself is its own decision and is left alone.

**Optional.** Unwired, nothing resolves and a conversation carrying refs still
runs — each ref-sourced image dropped with a warning. A host that never writes
refs never notices the port exists.

### `ToolSetContributor`

[`ports/tool-contributor.ts`](../packages/host/src/ports/tool-contributor.ts)

A source of tools for a run, contributed per run rather than registered
once at boot — which tools exist depends on what the chat is bound to and
what the user is allowed to do, both of which change between turns.

**`namespace` is required** — a bare `^[a-z][a-z0-9_-]*$` token. It is
attribution and reservation, **not a prefix**: tool names are never
rewritten (`TOOL_NAME_PATTERN` forbids dots, and a mechanical `ns__` rename
would change the name every existing tool is called by). What it buys is
that `agentkit`, `chat` and `mcp` are **reserved** — refused at staging
unless the contributor also sets the framework-internal `privileged: true`
(only `@agentkit/mcp-client` does, for `mcp`) — and that every staged tool
records its owner, so a `ToolGuard`, the `ToolCatalog`, and a collision
error can all name it.

**Key invariant (fail closed on collisions)**: a tool name offered by two
*different* contributors fails the whole staging with `tool_name_collision`,
naming both namespaces. One of them quietly winning would mean the model is
shown one description and reaches the other implementation, with the
arguments it wrote for the first. A duplicate *within* one contributor stays
lenient (logged, that one tool dropped): there is no ambiguity of ownership
to fail over.

**Key invariant (unbound pruning)**: `unboundToolNames()` (optional)
declares which of a contributor's tools stay available when the chat has no
primary binding. `turn/registry-staging.ts`'s `stageRegistry` prunes by this
hook alone, never by a hardcoded list — only the contributor that wrote a
tool knows whether it can operate on nothing, and when *no* contributor
declares the hook, nothing is pruned (an absent declaration means "no
opinion", not "empty the registry").

**`dispose()`** (optional) releases what the contributor holds open.
`TurnRunner.disposeContributors()` calls it once per contributor at
shutdown; it is idempotent (a second signal is a no-op) and a throw is
logged, not rethrown — a shutdown that gives up halfway leaks more than the
error it reported.

**`ctx.chatId` is optional**, and absent in exactly one place: `ToolCatalog`,
which enumerates tools for `GET /v1/tools` and names no conversation. A
contributor that cannot answer without a chat returns nothing there.

**Example implementation**: `@agentkit/mcp-client`'s
`createMcpToolSetContributor` (`packages/mcp-client/src/contributor.ts`) —
namespace `mcp`, `privileged: true`, turns every connected MCP server's tools
into `AiTool`s, failing the whole contribution closed on a canonical-id
collision rather than silently dropping one. See
[`packages/mcp-client/README.md`](../packages/mcp-client/README.md).

### `ToolGuard`

[`ports/tool-guard.ts`](../packages/host/src/ports/tool-guard.ts)

Visibility and executability policy over whatever the contributors offered,
wired as `TurnRunnerDeps.toolGuards`. Two hooks, both optional, checked at
different moments:

- `isVisible(ctx)` runs at **registry staging**. A tool it hides is never
  advertised — not in the registry, never in the provider request, so the
  model cannot call it. This is the hook for "this deployment does not have
  that feature": an advertised-but-always-refused tool spends context on
  every turn and invites the model to keep trying.
- `canExecute(ctx)` runs at **call time**, on a tool that *was* advertised —
  the hook for state that moves within a run (a lock taken, a budget spent, a
  binding gone stale). A refusal becomes an `ok: false` tool result carrying
  `errorCode: "tool_guard_refused"`, `phase: "guard"` and the reason; it
  never throws, so the run completes and the `tool_call_id` stays balanced.

**Key invariant**: guards compose with **AND**, an absent hook is "no
opinion" (not "allow"), and order is not significant — every guard is asked
and the first refusal wins. `ToolGuardContext` carries the owning
`namespace`, the `AiToolDefinition`, the run's `bindings`, and `chatId` when
there is one.

**Optional.** Unwired, nothing is asked and every contributed tool is staged
and callable, exactly as before the port existed.

### `ToolCatalog`

[`ports/tool-catalog.ts`](../packages/host/src/ports/tool-catalog.ts)

Enumerate tools **without running a turn** — what `GET /v1/tools` needs and
what it answered 501 without. `listTools({ chatId })` reports what that
chat's next turn would be handed; `listTools()` reports the chat-independent
set (no bindings, so the unbound rules apply). Entries are
`{ namespace, definition }` — **definitions only**: handing out
`AiTool.execute` here would put a second, unguarded, unlogged call path next
to the run loop's.

**Key invariant**: the default implementation
(`createContributorToolCatalog`, `tools/contributor-tool-catalog.ts`)
answers by running the **same** `stageRegistry` the turn runner does, so
namespace checks, guards and unbound pruning all apply and the catalogue
cannot drift from what a run actually receives. A second enumeration path
that disagreed would be worse than the 501, because it looks authoritative.
It does not call `ContextProvider.refresh`: listing is a read, and
re-validating every binding because a UI opened a tool picker would make an
enumeration as expensive as a turn.

## Secrets, authorization, usage

### `SecretStore`

[`ports/secret-store.ts`](../packages/host/src/ports/secret-store.ts)

Keeps API keys and tokens out of the records that describe them. A
`ProviderConfig` is read, listed, and shipped to a UI freely; the key it
uses is resolved from a `ref` only at the moment a client is built
(`TurnRunner.withSecret`).

### `AuthorizationPort`

[`ports/authorization.ts`](../packages/host/src/ports/authorization.ts)

"Is this actor allowed near this resource?" — separate from `WritePolicy`,
which answers "may this apply without confirmation?" A desktop host wires a
permissive implementation; a multi-tenant service does not.

**Where it is enforced**: in the transport, **per route**.
`@agentkit/transport-http`'s handler consults it once per request, after
`deps.authenticate` has produced the principal and after the route has been
resolved, and before the route handler runs — so a refusal costs no store
read and no host call. The `subject` is the principal (an object verbatim,
anything else under `metadata.principal`), the `action` is `read` for `GET`
and `write` for every other method, and the `resource` comes from the route
table in
[`transport-http/src/authorize.ts`](../packages/transport-http/src/authorize.ts),
which is `satisfies Record<RestOperation, …>` so a route added to
`REST_ROUTES` cannot ship without one. `GET /v1/version` is the single
exemption. A refusal is a `403` problem with code `forbidden`, carrying the
decision's `reason` as its `detail`.

Nothing else in this repository calls the port. In particular the host layer
does not: `TurnRunner`, `TaskService` and `ProposalService` are reached
through a transport or by a host's own code, and a second consultation point
inside them could disagree with the first.

**A host that does not wire it gets no authorization at all** — `deps.authorize`
is optional and absent means every routed request proceeds. That is the
intended default for a single-user desktop embedding; a multi-tenant service
must supply the port (or filter in front of the handler).

### `UsageAuthorizer`

[`ports/usage-authorizer.ts`](../packages/host/src/ports/usage-authorizer.ts)

Spend control around provider calls: `authorize()` before, `record()`
after. Two methods rather than one because the interesting failure is
between them — a run authorized on an estimate and then far over budget
must still be recorded, so the next authorization can refuse.

**Where it is enforced**: in `TurnRunner`, **per provider pass**.
`authorize()` is called at the top of `runPass` — the one place a provider
call is made — so the first pass and every recovery pass (chat-only retry,
empty-response retry) are each asked, because each of them bills again. The
request carries the run id, the chat id, the resolved provider id and model,
and `estimatedPromptTokens`: the assembled prompt's characters divided by
four, a rule of thumb rather than a tokenizer result, which is why the port
documents the field as best-effort and optional.

A refusal never reaches the provider. The runner writes a `run.failed` event
carrying `errorCode: "usage_denied"` (and the decision's `reason` in its
message) onto the task's durable log, then throws `UsageDeniedError`, which
lands the task `failed` through the same failure path any other thrown error
does. `@agentkit/transport-http` maps that code to **429**, not 403 —
`UsageAuthorizationDecision.retryAfterMs` exists because a refilling quota
says yes later.

`record()` is called for every `run.usage` event the provider emits, after
that event is durable, with the provider's own numbers (never the estimate).
Every usage event is reported, including the non-final ones a streaming
provider sends mid-call: a recorder that only saw `finalForCall` would lose
the accounting for a call that died before it settled. The record carries
`finalForCall`, `source` and `step` from the event so the recorder can tell
those two kinds apart — sum the settled reports, treat the rest as a running
estimate for the same `callId` rather than adding them.

**A host that does not wire it gets no spend control** — `TurnRunnerDeps.usage`
is optional, and absent the runner asks nothing, records nothing, and behaves
exactly as it did before the port was enforced.

## System seams

[`ports/system.ts`](../packages/host/src/ports/system.ts)

`Clock` (`now()`/`nowIso()`), `IdGenerator` (one method per entity kind —
`taskId`, `attemptId`, `eventId`, `proposalId`, `operationId`, `messageId`,
`chatId` — so a fake can hand out readable per-kind sequences, and so a
`createChat`/`forkChat` that mints an id mints it through the same seam), and
`Logger`
(structured: `fields` is a flat bag, not a formatted string). Ports rather
than direct `Date`/`crypto`/`console` calls because the orchestrator's
correctness is defined in terms of them — lease expiry, idempotency keys,
ordering — and a test that cannot move the clock or pin an id cannot assert
any of it. `defaultClock` and `defaultIds` (UUID-backed, `crypto.randomUUID`)
are provided as the obvious real implementations.

## Proposal lifecycle

```
  create()  ──▶  pending

  pending   ──▶  approved              [approve()]
  pending   ──▶  rejected              [reject()]                      (terminal)
  pending   ──▶  invalidated           [invalidatePendingForRevision()] (terminal)

  approved  ──▶  applying              [apply(): claim]

  applying  ──▶  applied               [apply(): outcome ok | partial]  (terminal)
  applying  ──▶  failed                [apply(): outcome failed]        (terminal)
```

Source: `PROPOSAL_TRANSITIONS` in
[`packages/host/src/proposals/state-machine.ts`](../packages/host/src/proposals/state-machine.ts),
driven by `ProposalService`
([`packages/host/src/proposals/proposal-service.ts`](../packages/host/src/proposals/proposal-service.ts)).

Notes:

- **`applying` is durable, not a stack frame.** A process that dies
  mid-apply leaves the record there; `ProposalService.reconcileInterrupted`
  resolves it later by asking the applier what happened, never by guessing.
- **Manual vs. policy-approved vs. deny.** `ProposalDecision.actor` is
  `"user"` or `"policy"`. A `"policy"` decision MUST carry the `policyId`
  that authorized it; a `"user"` decision MUST NOT carry one — the audit
  trail must be able to tell an auto-applied write from a human-reviewed
  one apart. **A policy approval is never called "human approval"** in
  code or in these docs; `createProposalBuilderTool`'s auto-apply gate
  records it as `actor: "policy"` even though nothing paused for a person.
  A refusal (the policy says no, or no allowance covers the write) simply
  leaves the proposal `pending` — there is no separate "denied" status; it
  waits exactly like a write nobody has looked at yet.
- **`partial` is carried on the outcome, not a proposal status.** The
  proposal's terminal status is `applied` even when the outcome's `status`
  is `"partial"` — some operations landed, some did not, and the state
  machine treats "the write happened" (the fact) as distinct from "how much
  of it happened" (a property of `ApplyOutcome`, which the model and the UI
  read separately).
