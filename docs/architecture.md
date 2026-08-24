# Architecture

AgentKit is three layers plus a testing layer, and a workspace-private set of
reference implementations. Each layer only depends on the ones below it.

```
internal/reference-adapters   @agentkit/reference-adapters (workspace-private; not published)
  implements host's ports over bun:sqlite / in-memory Maps
        │
        ▼  depends on
--------------------------------------------------------------
packages/host          @agentkit/host
  Durable orchestration: TaskStore + kind-dispatched
  executors, TurnRunner as the chat.turn executor, port
  catalog, proposal lifecycle, write policy.
        │
        ▼  depends on
--------------------------------------------------------------
packages/core           @agentkit/core
  Pure in-process loop: provider client, tool registry
  + Ajv validation, runChat() and its invariants.
        │
        ▼  depends on
--------------------------------------------------------------
packages/contracts      @agentkit/contracts
  Wire DTOs + JSON Schemas (TypeBox). No behavior.
--------------------------------------------------------------

packages/testing         @agentkit/testing
  Mocks, fixtures, golden traces, store-conformance suite.
  Depends on contracts; core/host are peer deps (type-only
  in the package itself, real in its own tests).
```

Source: [`packages/contracts/src/index.ts`](../packages/contracts/src/index.ts),
[`packages/core/src/index.ts`](../packages/core/src/index.ts),
[`packages/host/src/index.ts`](../packages/host/src/index.ts),
[`packages/testing/src/index.ts`](../packages/testing/src/index.ts).

## The three layers

### contracts — wire shapes

`@agentkit/contracts` owns every DTO that crosses a process or a storage
boundary: run events, tool definitions/calls/results, provider config,
prompt shapes, source refs, context bindings. Each is declared once as a
[TypeBox](https://github.com/sinclairzx81/typebox) schema (`<Name>Schema`)
and its TypeScript type is derived via `Static<typeof <Name>Schema>`, so the
compile-time type and the runtime JSON Schema cannot drift apart. A handful
of documented divergences layer a hand-written type over the schema where a
type parameter outruns what `Static<>` can express (`AiToolResult<T>`,
`AiSourceRef<K>`, `AiContextBinding<K>`, `AiRunWarningEvent`, `AiRunEvent`) —
see [`docs/contracts.md`](contracts.md).

It also carries the REST v1 surface (`src/rest.ts`): the route table and the
request/response DTOs an HTTP adapter would serialize, as types and schemas
only — the DTOs are projections of the host records with the orchestrator's
internals removed (see
[`docs/contracts.md`](contracts.md#rest-v1-surface)). No adapter implements
them here.

This package has zero runtime dependency beyond `@sinclair/typebox`. It has
no notion of a run loop, a queue, or a database.

### core — the pure loop

`@agentkit/core` is an in-process, single-invocation chat loop with tool
calling. Its three pieces:

- **Provider client** (`providers/`) — `AiProviderClient` is the interface;
  `OpenAiCompatibleClient` is the shipped implementation, talking to any
  OpenAI-compatible `/chat/completions` endpoint over SSE
  ([`packages/core/src/providers/openai-compatible.ts`](../packages/core/src/providers/openai-compatible.ts)).
  `providers/presets.ts` ships defaults for five provider kinds (`openai`,
  `openrouter`, `lmstudio`, `omlx`, `openai-compatible`); the vocabulary
  itself (`AiProviderKind`) is an open string, so a host can register a
  provider this package has never heard of. It also maps a message's
  provider-neutral content parts onto the wire shape OpenAI-compatible
  servers expect — native parts for `user`/`assistant`, flattened to text
  (with a `multimodal_flattened` warning) for `system`/`tool` — see
  [`docs/contracts.md`](contracts.md#message-content-parts).
- **Tool registry** — `AiToolRegistry` (`tools/registry.ts`) compiles each
  tool's `inputSchema` into an Ajv validator once, at registration, and
  reuses it on every call. `tools/validation.ts` maps Ajv errors to
  `{ path, message }`; `tools/limits.ts` resolves per-run output byte/item
  budgets and does UTF-8-safe truncation.
- **`runChat()`** — the loop itself
  ([`packages/core/src/runs/run-loop.ts`](../packages/core/src/runs/run-loop.ts)).
  An async generator: it yields `AiRunEvent`s and returns a `RunChatResult`
  (`{ runId, terminal, appendedMessages, iterations }`). It does not mutate
  its caller's `messages` array — it copies, appends, and hands back only
  what it appended, so the same history can be retried or run twice.

`@agentkit/core` has no storage, no queue, no HTTP framework. Nothing in it
survives a process restart; that is `@agentkit/host`'s job.

### host — durable orchestration

`@agentkit/host` wraps `runChat()` in the machinery an embedding app needs
to run it durably, across process restarts and retries:

- **`TurnRunner`** ([`packages/host/src/turn/turn-runner.ts`](../packages/host/src/turn/turn-runner.ts))
  — a `TaskWorker`, and (via the thin `ChatTurnExecutor` adapter) the
  `TaskExecutor` for task kind `chat.turn`. It turns a submitted chat
  message into a durable task, drives `runChat()`, appends every event to
  the task's log, projects it into conversation state, and runs the
  chat-only / empty-response recovery passes (`turn/retry.ts`).
- **The port catalog** ([`packages/host/src/ports/`](../packages/host/src/ports/))
  — interfaces an embedding host implements: `TaskStore` (kind, payload,
  leases, fencing, the event log), `ProposalStore`, `ConversationStore`,
  `ProviderStore`, `SettingsStore`, `OutboxStore`, `TaskRunner`,
  `WritePolicy`, `ProposalApplier`, and more. See [`docs/ports.md`](ports.md).
- **Proposals** ([`packages/host/src/proposals/`](../packages/host/src/proposals/))
  — the staged-write pipeline: `ProposalService` drives
  `pending → approved → applying → applied|failed`, action-id idempotency
  (`proposals/action-id.ts`), and `createProposalBuilderTool` — the wrapper
  a host's write tools are built on.
- **Policy** ([`packages/host/src/policy/session-write-policy.ts`](../packages/host/src/policy/session-write-policy.ts))
  — `SessionWritePolicy`, an in-memory `WritePolicy` implementation whose
  standing allowances live only for the process lifetime. Only the write tool
  consults it (`createProposalBuilderTool`, on the proposal it just staged);
  `TurnRunner` takes no policy dependency, so there is exactly one place the
  auto-apply question is answered.
- **Bootstrap** ([`packages/host/src/bootstrap.ts`](../packages/host/src/bootstrap.ts))
  — `recoverOnBoot`: `TaskRunner.recover()` then
  `ProposalService.reconcileInterrupted()`, in that order, before any worker
  starts claiming.

`@agentkit/host` depends on `@agentkit/core` and `@agentkit/contracts`; it
implements no storage itself — every store is a port, implemented by the
embedding host or by the reference adapters under `internal/`.

## Event flow

1. **Core stamps the base fields.** `createEventStamper()`
   ([`packages/core/src/events.ts`](../packages/core/src/events.ts)) assigns
   `contractVersion`, a fresh `eventId`, and the next `seq` to every event a
   `runChat()` invocation yields — both loop-originated events and ones
   re-yielded from the provider client. `seq` starts at `RunChatInput.firstSeq`
   (default 0).
2. **Host continues `seq` across retries.** One task id (its value is what
   rides the wire as `AiRunEvent.runId`) can span several `runChat()` passes
   (a chat-only retry, an empty-response retry — see `turn/retry.ts`).
   Before each pass, `TurnRunner.runPass` reads `TaskStore.nextSeq(taskId)`
   and passes it as `firstSeq`, so every pass continues the same unbroken
   sequence instead of restarting at 0
   ([`packages/host/src/turn/turn-runner.ts`](../packages/host/src/turn/turn-runner.ts)).
3. **`TaskStore.appendEvents` is the enforcement point.** An implementation
   MUST reject a stale `leaseToken` (`LeaseLostError`) and MUST reject a
   non-monotonic `seq` (`SeqConflictError`); it MUST NOT re-stamp `seq` —
   numbering belongs to whichever emitter owns the pass, not the store
   ([`packages/host/src/ports/task-store.ts`](../packages/host/src/ports/task-store.ts)).
4. **The durable log is canonical.** `TurnRunner.projectEvent` appends to the
   task's log *before* reflecting the event into conversation state, so a
   projection failure can never erase what happened. Publishing to a live
   transport (SSE, a websocket) is a separate, retryable step through the
   `OutboxStore` port — it replays the log outward; it is not itself the
   source of truth
   ([`packages/host/src/ports/outbox-store.ts`](../packages/host/src/ports/outbox-store.ts)).
5. **`createTaskEventWriter` numbers events outside a chat pass.** Non-chat
   executors, and any host-originated event that is not part of a live
   `runChat()` pass, stamp `seq`/`eventId`/`timestamp`/`contractVersion`/
   `attemptId` through it and append under the lease
   ([`packages/host/src/tasks/task-event-writer.ts`](../packages/host/src/tasks/task-event-writer.ts)).
   It is explicitly barred from use **inside** a chat pass — there, core's
   `createEventStamper` owns numbering, and two counters numbering against
   the same log would interleave into one stream.

## Task / attempt / lease model

```
  queued            ──▶ running
  queued            ──▶ cancelled

  running           ──▶ waiting_approval
  running           ──▶ completed | failed | cancelled

  waiting_approval   ──▶ running
  waiting_approval   ──▶ completed | failed | cancelled

  completed / failed / cancelled   (terminal — no outgoing edges)
```

Source: `TASK_TRANSITIONS` in
[`packages/host/src/ports/task-store.ts`](../packages/host/src/ports/task-store.ts).
Note there is **no `running → queued` edge** — a task that started never goes
back to the queue. Note also that **nothing in this repository produces
`waiting_approval`**: `TurnRunner` lets a staged write return `pending` to the
model and completes the task. The state is reserved for hosts that park a
task on a human decision and resume it afterwards; its transitions exist so
such a host does not have to fork the table.

`AttemptStatus` is `running → completed | failed | abandoned | cancelled`.
`abandoned` is specifically the crash outcome: a lease expired while the
attempt was still `running`, so recovery ends it that way rather than
guessing success or failure.

**Recovery**, as implemented by the reference `SingleProcessTaskRunner`
([`internal/reference-adapters/src/task-runner/single-process-task-runner.ts`](../internal/reference-adapters/src/task-runner/single-process-task-runner.ts)):

1. `recover()` calls `TaskStore.expireStaleLeases(now)`.
2. Each expired lease's attempt is ended `abandoned`.
3. If the task's `attemptCount` has not hit `maxAttempts`, a **new attempt is
   started on the same task** — same task id, fresh lease with a strictly
   higher `fencingToken`, event `seq` picking up where the log left off.
   Tasks never transition back to `queued`; a retry is a new attempt, not a
   re-enqueue.
4. Once `attemptCount >= maxAttempts`, the task is marked dead-lettered
   (`TaskStore.markDeadLettered`) and finalized `failed` with a poison
   reason — the queue stops feeding it work.

Ownership and fencing: `Lease { taskId, attemptId, ownerId, leaseToken,
fencingToken, expiresAt }`. `leaseToken` proves current ownership on every
write; `fencingToken` is monotonic across *all* leases ever issued for a
task, so a worker that paused and woke up believing it still owns the task
is rejected by comparison even if it manages to re-acquire.

## Task kinds and executors

A task's `kind` says what work it is and which code runs it — statuses,
attempts, and leases above are kind-agnostic; dispatch is where a kind
becomes an executable.

- **`TaskExecutor` / `TaskExecutionContext`**
  ([`packages/host/src/tasks/task-executor.ts`](../packages/host/src/tasks/task-executor.ts))
  — the unit of work behind one kind: `{ kind, execute(ctx) }`, where `ctx`
  carries the already-loaded `TaskRecord` plus `attemptId`/`leaseToken`/
  `signal`. The record is handed down rather than an id so every executor
  works from the one fetch-and-guard the dispatcher already did, instead of
  each re-reading the row and risking a different answer to "is this still
  mine to run?".
- **`ExecutorRegistry`**
  ([`packages/host/src/tasks/executor-registry.ts`](../packages/host/src/tasks/executor-registry.ts))
  — the kind → `TaskExecutor` table one worker process dispatches through.
  `register` throws on a duplicate kind at boot, rather than letting the
  later import silently win; `kinds()` lists what is registered, for a
  deployment that filters `ClaimNextInput.kinds` so a box only claims work
  it can run.
- **`createDispatchingWorker(registry, deps)`** — the `TaskWorker` a host
  hands to `TaskRunner.startWorker`: it loads the task, performs the
  `queued → running` guard for the direct-execute path, and routes to the
  registered executor. **An unknown kind is a terminal failure
  (`ExecutorNotFoundError`), never a dead-letter** — dead-letter stays
  reserved for attempts that die without a clean terminal outcome, and a
  kind nobody registered is a cleanly-diagnosed wiring mistake instead.
- **`TaskService`**
  ([`packages/host/src/tasks/task-service.ts`](../packages/host/src/tasks/task-service.ts))
  — the generic submission path: `createTask(tx, input)` composes inside a
  host transaction and never enqueues; `dispatch(task)` is the post-commit
  poke; `submitTask(input)` does both and is idempotent per caller-supplied
  `taskId`. **Dispatch happens strictly after the transaction commits** —
  enqueuing from inside the transaction callback risks the claim loop
  claiming a row that a rollback then deletes out from under it (the
  `bun:sqlite` join-transaction hazard on `AssistantStore.transaction`, see
  [`docs/ports.md`](ports.md#assistantstore-aggregate)).
- **`createTaskEventWriter`** — see item 5 of [Event flow](#event-flow)
  above.
- **`CHAT_TURN_TASK_KIND`**
  ([`packages/host/src/tasks/kinds.ts`](../packages/host/src/tasks/kinds.ts))
  — `"chat.turn"`, the kind `TurnRunner.submitMessage` creates and
  `ChatTurnExecutor` runs. The `chat.*` and `agentkit.*` prefixes are
  reserved for the framework; everything else belongs to the host.

## At-least-once delivery, idempotent effects

Nothing in this codebase claims exactly-once semantics. The stance is
at-least-once delivery paired with idempotent effects, enforced at each
layer that can duplicate work:

- `TaskRunner.enqueue` is idempotent per `taskId`
  ([`packages/host/src/ports/task-runner.ts`](../packages/host/src/ports/task-runner.ts)):
  a redelivered enqueue for a task that already left `queued` is a no-op.
- `TaskService.submitTask` is idempotent per caller-supplied `taskId`
  ([`packages/host/src/tasks/task-service.ts`](../packages/host/src/tasks/task-service.ts)):
  a duplicate submit under the same id and kind re-pokes the queue and
  returns the existing task instead of creating a second one; a minted id
  that collides is treated as a broken `IdGenerator`, not a redelivery, and
  rethrows.
- `ProposalStore.recordOutcome` is idempotent per `operationId`
  ([`packages/host/src/ports/proposal-store.ts`](../packages/host/src/ports/proposal-store.ts)):
  a second call with the same id returns the first recorded outcome and
  never re-invokes the applier.
- `ProposalStore.create` enforces `UNIQUE(scopeKey, actionId)` for
  model-supplied idempotency keys, except among proposals that never wrote
  anything (`rejected`/`invalidated` — `ACTION_ID_RELEASING_STATUSES`).
- `ProposalService.reconcileInterrupted` resolves every proposal a crash
  left in `applying` by asking the applier what actually happened
  (`ProposalApplier.getOutcome`), rather than guessing; "we cannot prove it
  landed" is treated as failure, because a retried write that silently
  succeeded once is the one outcome nobody can undo
  ([`packages/host/src/proposals/proposal-service.ts`](../packages/host/src/proposals/proposal-service.ts)).

## Loop invariants

`runChat()` preserves these across every code path, including cancellation
and provider failure (source:
[`packages/core/src/runs/run-loop.ts`](../packages/core/src/runs/run-loop.ts)):

- **Balanced tool history.** The assistant message pushed to `messages`
  lists *every* tool call the turn produced — including ones over the
  `maxToolCallsPerIteration` cap — because a provider rejects an orphan
  `tool_call_id` on the next turn. Every one of those ids then gets a
  matching `role: "tool"` message, whether it ran, was skipped, or was
  cancelled.
- **All tool calls listed, including over-cap.** `skippedToolCalls` (the
  slice past the cap) still gets a `run.tool.failed` event with
  `errorCode: "tool_call_cap"` and a matching tool message — never silently
  dropped.
- **`balanceCancelled` on abort.** If cancellation arrives mid-iteration,
  every tool call at or after the current one — capped and skipped alike —
  is failed with `errorCode: "cancelled"` before `run.cancelled` is emitted,
  so no `tool_call_id` is left unanswered in the replayed history.
- **No abort check after a tool executes.** The run-loop's own comment:
  a tool that has already run has real side effects, so its result MUST be
  emitted and appended even if cancellation arrived while it was in flight —
  discarding it would make a replay say "cancelled" while the world had
  already changed.
- **`partial` wins in the model-facing envelope.** `buildEnvelope()` maps a
  tool result's `status` to `"partial"` when the tool reported it, even when
  `ok` is `false` — a partial apply is not folded into a generic `"error"`,
  because the model correcting its own work needs to know some of it landed.
- **A provider-emitted `run.failed` ends the pass without `run.completed`.**
  If the provider client already yielded `run.failed` during streaming, the
  loop returns `finish("failed")` directly rather than double-emitting a
  terminal event.

## See also

- [`docs/contracts.md`](contracts.md) — the event vocabulary, warning codes,
  tool envelope, and TypeBox conventions.
- [`docs/ports.md`](ports.md) — the full port catalog and the proposal
  lifecycle diagram.
- [`docs/non-goals.md`](non-goals.md) — what this repository deliberately
  does not include yet.
