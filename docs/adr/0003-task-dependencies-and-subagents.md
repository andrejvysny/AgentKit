# ADR 0003 — Task dependencies and subagent spawning

**Status:** accepted, implemented (2026-08-24)
**Contract impact:** NONE. `CONTRACT_VERSION` stays `0.2.0` — this phase adds
fields to host-internal records (`TaskRecord`) and reference-adapter schema,
not to `@agentkit/contracts`.

## Problem

[ADR 0001](0001-generic-task-foundation.md) generalized the durable unit from
a chat run to a kind-dispatched task, and named this work explicitly as
deferred scope: "Task dependencies (`dependsOn`, parent/child) can be added
as claimability rules in `claimNext` without schema conflict … the design
must include the failure cascade task-system itself lacked." Without them, a
host that wants one task to wait on another (a continuation) or one task to
fan work out to several children (a subagent) has no primitive — every
embedding would hand-roll its own polling table or in-memory join, on top of
a store that already has leases, CAS transitions and dead-lettering to do
exactly this kind of coordination correctly.

## Evidence

- **task-system** aged every queue unconditionally and without a cap (see
  `internal/reference-adapters/src/task-aging.ts`'s module doc): wait long
  enough and a background sweep outranks an interactive turn, on a queue
  whose owner never asked for that trade. It also cascaded **cancel** to
  dependents but left dependents of a **failed** parent stuck `waiting`
  forever — the asymmetry this phase's dependency gate had to not repeat.
- AgentKit's store layer already had the primitives this needs: CAS
  transitions (`assertTaskTransition`), leases/fencing, and a `claimNext`
  that already filtered on `(status, scopeId, availableAt)`. Dependency
  gating is one more predicate on the same claim path, not new
  infrastructure — the shape ADR 0001's Consequences predicted.

## Decision

1. **Two distinct edges, not one.** `parentTaskId` is **lineage**:
   `TaskExecutionContext.spawnChild` presets it, and it is not a dependency
   or a lifecycle coupling — a child runs the moment the queue can claim it,
   whether or not its parent is still running, because a parent usually
   spawns children precisely so they can proceed without it. `dependsOn` is
   the **claim gate**: ids that must reach `completed` before this task may
   be claimed. Lineage buys `TaskStore.listChildren` ("what did this task set
   off?") and `TaskService.cancelTask`'s cascade ("cancel this whole
   branch"); the gate buys ordering. Conflating them would mean a host that
   only wants to fan out gets forced waiting, or a host that only wants to
   wait loses cancellation cascade.
2. **The dependency graph is a DAG by construction, not by detection.**
   `TaskStore.createTask` MUST reject an unknown `parentTaskId` or
   `dependsOn` entry with `UnknownDependencyError`, and a task naming itself
   is rejected the same way. Because every dependency must already exist
   when the dependent is written, an edge can only ever point backward in
   creation order — there is no sequence of writes that produces a cycle,
   and nothing can add one later (`dependsOn` is immutable after create).
   Cycles are unrepresentable, not merely checked for.
3. **Claimability-based lazy settle, enforced in `claimNext`, never a
   reaper.** `evaluateTaskDependencies` (`packages/host/src/ports/task-store.ts`)
   is the one function every adapter calls to reach the same verdict from
   the same facts: a dependency that failed or was dead-lettered settles the
   dependent `failed` with `error: "dependency_failed: <id>"`; a cancelled
   dependency settles the dependent `cancelled` (no `error` — a cancellation
   is not a failure, and recording one as an error would make every "why did
   this fail?" dashboard lie); anything still in flight blocks the claim
   without touching the row. A bad dependency beats a pending one regardless
   of order, and among several bad ones the first in `dependsOn` order
   decides, so the verdict is a function of the record, not of scan order.
   Settling happens ON THE CLAIM PATH and nothing is ever re-enqueued — a
   chain of dependents settles over successive claim calls, each sweep
   resolving what the previous one unblocked, and the store stays the single
   writer deciding what is runnable.
4. **`queued → failed` added to `TASK_TRANSITIONS`, for exactly this one
   caller.** A task whose dependency died without ever starting still has to
   be able to end `failed` from `queued` — the dependency cascade is the
   only code path that uses this edge; nothing else in the table changed.
5. **`TaskService.cancelTask` is a breadth-first cascade over `parentTaskId`,
   never `dependsOn`.** A queued descendant is CAS-cancelled directly in the
   store (nobody has claimed it, so there is no execution to stop). A
   running or `waiting_approval` descendant is asked to stop through
   `taskRunner.requestCancel` — cooperative, never forced terminal: flipping
   a row to `cancelled` under a worker still executing would produce a task
   the store calls finished while its executor keeps writing events, two
   answers to "did this run?". Tasks merely *waiting on* the cancelled task
   (via `dependsOn`) are not touched by this cascade at all — they settle
   themselves on their next claim attempt, because `evaluateTaskDependencies`
   reads a cancelled dependency as "cancel the dependent" already. One
   cascade per edge type, each enforced where that edge lives.
6. **`TaskExecutionContext.spawnChild`**, wired by `createDispatchingWorker`
   only when it was constructed with a `TaskService`. It presets
   `parentTaskId` to the task being executed, so lineage cannot be forged or
   forgotten by the executor — the dispatcher names the parent, the executor
   only names the work. It is optional (`ctx.spawnChild?.(...)`) because
   spawning is a submit (create-then-dispatch against a queue), a dependency
   a bare `ExecutorRegistry` does not have.
7. **The continuation pattern is `dependsOn`, not a new join primitive.** A
   parent that must wait for children it spawned submits a further task
   whose `dependsOn` names them; there is no separate "await my children"
   API, because the dependency gate already expresses it.
8. **Priority aging becomes opt-in.** The formula is preserved verbatim from
   task-system (`effective = priority + min(maxBonus, floor(waitMs /
   intervalMs) × bonus)`, `internal/reference-adapters/src/task-aging.ts`),
   but `agingBonus` now defaults to `0` (off — ordering is exactly `priority
   DESC, enqueuedAt ASC` until a host opts in) and `agingMaxBonus` caps how
   far a waiting task may climb once it does. Measured against
   `ClaimNextInput.now` — the caller's clock, not the store's own — for the
   same reason `availableAt` filtering already used it: the value that ages
   a row and the value a test can move must be the same one.
9. **`progress` is an overwritten snapshot, not an event.**
   `TaskStore.updateProgress` (lease-gated with the same `LeaseLostError`
   check `appendEvents` makes) REPLACES `TaskRecord.progress` wholesale and
   never touches the event log. A heartbeat percentage does not belong in
   the durable, replayed record every consumer folds through — only the
   latest value matters, and losing an intermediate one costs nothing.
10. **`SCHEMA_V3`**: `parent_task_id`, `depends_on` (JSON array — not an edge
    table; edges are immutable and read only for the one task being gated,
    so a join table buys nothing and costs a write per submit), and
    `progress` columns on `tasks`, plus `idx_tasks_parent`
    (`listChildren`'s only query, and what the cancel cascade walks once per
    node). `PRAGMA user_version` bumped to 3; a v2 dev database is rejected
    fail-closed, same precedent as ADR 0001's v1 → v2 move.

## Alternatives considered

- **Eager cascade / a background reaper that walks dependents on every
  failure.** Rejected: a second writer racing the claim path over the same
  rows, which must independently reach the identical verdict `claimNext`
  would have reached anyway — maintaining the logic twice to save nothing.
  Lazy, on-claim settlement keeps the store the single decider of what is
  runnable.
- **Cycle detection performed at claim time (or by a background walk)
  instead of at creation.** Rejected: a queue that can accept a cycle and
  only notice it later is a queue that can deadlock on data it already
  committed — a page at 3am, not a validation error at write time. Requiring
  every dependency to pre-exist makes a cycle unrepresentable rather than
  merely detected.
- **`dependsOn` as re-enqueue** (task-system's model: a paused task returns
  to `queued` when its dependency finishes). Rejected: this is exactly
  task-system's own defect list per ADR 0001 (an in-memory queue as
  scheduling truth, retry-by-re-enqueue). `TASK_TRANSITIONS` has no `running
  → queued` edge and this phase does not add one; a dependent is either
  blocked in place or settled, never sent back through `queued` a second
  time.

## Consequences

- Subagent delegation is "spawn + a continuation task", not new
  infrastructure — the framing ADR 0001's Consequences predicted holds.
- A host that wants task-system's old always-on aging now sets `agingBonus`
  (and probably `agingMaxBonus`) explicitly at adapter construction; the
  default ordering changed to plain `priority DESC, enqueuedAt ASC`.
- `progress` is not visible through the event log or any SSE stream — a
  consumer that wants live progress reads `TaskRecord.progress` directly
  (polling, or a future REST projection); streaming it into a parent's own
  log is explicitly deferred (see below).
- The sqlite reference schema moved to v3; a v2 dev database is rejected
  with instructions to recreate it, not migrated in place.
- `@agentkit/testing`'s `store-conformance.ts` suite grew substantially to
  cover dependency gating, the cascade, `spawnChild`, and aging on both
  reference adapters — a new adapter is graded against the same behavior.

## Out of scope (deliberate)

Streaming child-task progress into a parent's own event log (progress stays
a per-task snapshot, not fanned upward — see `docs/roadmap.md`'s Later
list); durable cross-process forced cancellation of a running task (still
cooperative-only, unchanged from ADR 0001/`docs/non-goals.md`); a dedicated
"join all children" primitive beyond the `dependsOn` continuation pattern;
dependency gating for a distributed (non-reference) adapter.
