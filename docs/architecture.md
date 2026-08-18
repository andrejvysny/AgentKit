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
  Durable orchestration: TurnRunner, port catalog
  (RunStore/ProposalStore/...), proposal lifecycle,
  session write policy.
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
  provider this package has never heard of.
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
  — a `TaskWorker` that turns a submitted chat message into a durable run,
  drives `runChat()`, appends every event to the run's log, projects it into
  conversation state, and runs the chat-only / empty-response recovery
  passes (`turn/retry.ts`).
- **The port catalog** ([`packages/host/src/ports/`](../packages/host/src/ports/))
  — interfaces an embedding host implements: `RunStore` (leases, fencing,
  the event log), `ProposalStore`, `ConversationStore`, `ProviderStore`,
  `SettingsStore`, `OutboxStore`, `TaskRunner`, `WritePolicy`,
  `ProposalApplier`, and more. See [`docs/ports.md`](ports.md).
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
2. **Host continues `seq` across retries.** One run id can span several
   `runChat()` passes (a chat-only retry, an empty-response retry — see
   `turn/retry.ts`). Before each pass, `TurnRunner.runPass` reads
   `RunStore.nextSeq(runId)` and passes it as `firstSeq`, so every pass
   continues the same unbroken sequence instead of restarting at 0
   ([`packages/host/src/turn/turn-runner.ts`](../packages/host/src/turn/turn-runner.ts)).
3. **`RunStore.appendEvents` is the enforcement point.** An implementation
   MUST reject a stale `leaseToken` (`LeaseLostError`) and MUST reject a
   non-monotonic `seq` (`SeqConflictError`); it MUST NOT re-stamp `seq` —
   numbering is core's job, not the store's
   ([`packages/host/src/ports/run-store.ts`](../packages/host/src/ports/run-store.ts)).
4. **The durable log is canonical.** `TurnRunner.projectEvent` appends to the
   run's log *before* reflecting the event into conversation state, so a
   projection failure can never erase what happened. Publishing to a live
   transport (SSE, a websocket) is a separate, retryable step through the
   `OutboxStore` port — it replays the log outward; it is not itself the
   source of truth
   ([`packages/host/src/ports/outbox-store.ts`](../packages/host/src/ports/outbox-store.ts)).

## Run / attempt / lease model

```
  queued            ──▶ running
  queued            ──▶ cancelled

  running           ──▶ waiting_approval
  running           ──▶ completed | failed | cancelled

  waiting_approval   ──▶ running
  waiting_approval   ──▶ completed | failed | cancelled

  completed / failed / cancelled   (terminal — no outgoing edges)
```

Source: `RUN_TRANSITIONS` in
[`packages/host/src/ports/run-store.ts`](../packages/host/src/ports/run-store.ts).
Note there is **no `running → queued` edge** — a run that started never goes
back to the queue. Note also that **nothing in this repository produces
`waiting_approval`**: `TurnRunner` lets a staged write return `pending` to the
model and completes the run. The state is reserved for hosts that park a run on
a human decision and resume it afterwards; its transitions exist so such a host
does not have to fork the table.

`AttemptStatus` is `running → completed | failed | abandoned | cancelled`.
`abandoned` is specifically the crash outcome: a lease expired while the
attempt was still `running`, so recovery ends it that way rather than
guessing success or failure.

**Recovery**, as implemented by the reference `SingleProcessTaskRunner`
([`internal/reference-adapters/src/task-runner/single-process-task-runner.ts`](../internal/reference-adapters/src/task-runner/single-process-task-runner.ts)):

1. `recover()` calls `RunStore.expireStaleLeases(now)`.
2. Each expired lease's attempt is ended `abandoned`.
3. If the run's `attemptCount` has not hit `maxAttempts`, a **new attempt is
   started on the same run** — same run id, fresh lease with a strictly
   higher `fencingToken`, event `seq` picking up where the log left off.
   Runs never transition back to `queued`; a retry is a new attempt, not a
   re-enqueue.
4. Once `attemptCount >= maxAttempts`, the run is marked dead-lettered
   (`RunStore.markDeadLettered`) and finalized `failed` with a poison
   reason — the queue stops feeding it work.

Ownership and fencing: `Lease { runId, attemptId, ownerId, leaseToken,
fencingToken, expiresAt }`. `leaseToken` proves current ownership on every
write; `fencingToken` is monotonic across *all* leases ever issued for a
run, so a worker that paused and woke up believing it still owns the run is
rejected by comparison even if it manages to re-acquire.

## At-least-once delivery, idempotent effects

Nothing in this codebase claims exactly-once semantics. The stance is
at-least-once delivery paired with idempotent effects, enforced at each
layer that can duplicate work:

- `TaskRunner.enqueue` is idempotent per `runId`
  ([`packages/host/src/ports/task-runner.ts`](../packages/host/src/ports/task-runner.ts)):
  a redelivered enqueue for a run that already left `queued` is a no-op.
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
