# ADR 0001 — Generic task foundation: run → task, kind-dispatched executors

**Status:** accepted, implemented (2026-08-24)
**Supersedes:** the chat-specific `RunStore`/`RunRecord` port shape from the
original extraction (commits `b5a7a07`…`fb62545`).

## Problem

AgentKit's durable execution machinery — leases with monotonic fencing
tokens, attempts, the seq-ordered event log, crash recovery, dead-lettering —
was hardwired to one workload: a chat turn. `RunRecord` carried a required
`chatId`, and `TurnRunner` was the only thing a `TaskRunner` could execute.
Every planned capability that needs background execution (subagent
delegation, scheduled agents, indexing/ingest jobs, distributed workers)
would either duplicate that machinery or bolt itself awkwardly onto a
chat-shaped record.

## Evidence

- **task-system** (the predecessor repo this framework supersedes) proved the
  demand for a generic `kind → executor` model — its `ExecutorRegistry` +
  `TaskExecutionContext` is the shape application code wants — but its
  runtime had defects AgentKit's model already fixes: an awaited dispatch
  loop that serialized "concurrency 3" to one task, `retryable: !aborted`
  blanket retry classification, retry-by-re-enqueue (`paused → queued`), an
  in-memory queue as the source of scheduling truth (queued tasks lost on
  crash), and chunk writes with no ownership gating.
- **OneMind**'s task orchestration (same lineage, more mature) is durable per
  state transition but single-process by declared scope; no leases/fencing.
  Its value here is the executor/queue vocabulary, not the durability model.
- **AgentKit's own store layer was already generic in all but name**: nothing
  in `claimNext`/`transitionRun`/lease handling/dead-letter read `chatId`;
  the claim index was `(status, scope_id, available_at)`. The chat coupling
  was one required column and one hardcoded worker.

## Decision

1. **The durable unit is a task.** `RunStore` → `TaskStore`
   (`packages/host/src/ports/task-store.ts`), `RunRecord` → `TaskRecord
   { taskId, kind, scopeId, payload, … }`. Statuses, transitions, attempts,
   leases, fencing, seq rules, dead-letter semantics: unchanged, renamed
   (`RUN_TRANSITIONS` → `TASK_TRANSITIONS`, error code
   `invalid_run_transition` → `invalid_task_transition`).
2. **`chatId` leaves the generic record.** It lives in the `chat.turn`
   payload. A kind-specific nullable column on the generic record is exactly
   the schema smell the kind/payload split exists to remove.
3. **Kind-dispatched execution.** `TaskExecutor { kind, execute(ctx) }`,
   `ExecutorRegistry`, and `createDispatchingWorker(registry)` feeding the
   existing `TaskRunner.startWorker`. The dispatcher owns fetch, the
   not-executable guard, and the `queued → running` fallback; executors
   receive the already-loaded `TaskRecord`. An unknown kind is a
   **cleanly-diagnosed terminal failure** (`executor_not_found` → task
   `failed`), never a dead-letter — dead-letter remains reserved for poison
   (attempts dying without a terminal outcome).
4. **Chat turn is task kind `"chat.turn"`.** `TurnRunner` keeps its public
   API (`submitMessage`, legacy `execute`) and gains `ChatTurnExecutor`, a
   thin `TaskExecutor` adapter. Kind prefixes `chat.*` and `agentkit.*` are
   reserved for the framework.
5. **`TaskService`** is the generic submission path, split to respect
   transaction discipline: `createTask(tx, …)` composes inside a host
   transaction and never enqueues; `dispatch(…)` is the post-commit enqueue
   poke; `submitTask(…)` composes both and is idempotent per `taskId`
   (duplicate submit of the same kind re-pokes the idempotent enqueue and
   returns the existing record). Enqueueing inside the transaction callback
   is forbidden: over `bun:sqlite`, concurrent store calls join an open
   transaction, so the claim loop could claim a row that then rolls back.
6. **The event log is envelope-typed.** `TaskStore.appendEvents`/`listEvents`
   are typed on `TaskEventEnvelope` (`@agentkit/contracts`), the minimal
   shape the store actually orders (`seq`) and dedups (`eventId`).
   `AiRunEvent` is the `chat.turn` vocabulary and structurally satisfies the
   envelope; chat events keep their `runId` field (its value is the task id).
   Non-chat executors emit their own vocabularies via `createTaskEventWriter`,
   which is explicitly barred from chat passes — inside a pass, core's
   `createEventStamper` owns seq numbering, and two counters interleaved into
   one stream is the bug that rule prevents.
7. **`ClaimNextInput.kinds?`** filters claims for deployments whose worker
   pools register different executor sets (needed before distributed
   adapters; cheap now, breaking later).
8. **Wire vocabulary is untouched.** `AiRunEvent` (incl. `runId`), golden
   traces' shape, REST v1 `runs` routes/DTOs, `SubmitMessageResult.runId`
   stay as they are. "Run" remains the chat-facing word for one execution of
   the agent loop; "task" is the durable unit underneath it.

## Alternatives considered

- **Keep `RunStore`, add `kind` + optional `chatId`.** Rejected: leaves a
  chat column on every non-chat task forever and makes "run" mean two
  things. The rename was measured against its blast radius (all in-repo; the
  npm scope is unpublished, so there are zero external consumers) — this is
  the cheapest moment it will ever have.
- **A separate generic `TaskStore` beside `RunStore`.** Rejected: duplicates
  the lease/attempt/fencing machinery, the hardest-won code in the repo.
- **Deprecated type aliases (`RunStore = TaskStore`).** Rejected: aliases
  serve consumers, and there are none; they would only prolong the double
  vocabulary. The mapping is recorded here instead.
- **task-system's model (in-memory queue as truth, re-enqueue retry).**
  Rejected with evidence; see `internal/reference-adapters` doc comments,
  which record each defect and its fix.

## Consequences

- Any future background capability (subagents, scheduled work, ingest jobs)
  is "an executor + a kind", not new infrastructure.
- Task dependencies (`dependsOn`, parent/child) can be added as claimability
  rules in `claimNext` without schema conflict (see `docs/roadmap.md`, P4;
  the design must include the failure cascade task-system itself lacked).
- The sqlite reference adapter's schema moved to v2 (`PRAGMA user_version`);
  it is workspace-private and ships no migrations — a v1 dev database is
  rejected with instructions to recreate it.
- Task results have no dedicated column: by convention the terminal event a
  kind's vocabulary defines carries the result. Revisit if a polling-only
  consumer appears.
- External documentation (`docs/architecture.md`, `docs/ports.md`) was
  updated in the same change; any out-of-tree notes referring to `RunStore`
  should map names via this ADR.

## Out of scope (deliberate)

Task dependencies and subagent spawning (P4), priority aging (P4, formula
preserved from task-system), progress-as-mutable-state (`emitProgress`),
distributed adapters, durable cross-process cancellation.
