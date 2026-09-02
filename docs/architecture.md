# Architecture

AgentKit is three layers plus a testing layer, a set of reference adapter
packages implementing `host`'s ports, and a set of optional adapters beside
`host` (an MCP client, an MCP server, an HTTP transport, and a client +
React pair). Each layer only depends on the ones below it; the adapters
depend on `host` and nothing depends on them.

```
packages/adapters-memory       @agentkit/adapters-memory (reference adapter)
  every host storage port over in-memory Maps; tests and local dev

packages/adapters-sqlite       @agentkit/adapters-sqlite (reference adapter, Bun only)
  every host storage port over bun:sqlite; the durable store for a
  single-process host (multiple handles over one file: supported, tested)

packages/runner-local          @agentkit/runner-local (reference adapter)
  the TaskRunner port for one process: claim, execute, heartbeat,
  classified retry with backoff, dead-letter, recover

packages/mcp-client            @agentkit/mcp-client (optional adapter)
  bridges MCP servers' tools into a run as a ToolSetContributor

packages/transport-http        @agentkit/transport-http (optional adapter)
  fetch-standard REST v1 + SSE handler serving contracts' REST surface

packages/mcp-server            @agentkit/mcp-server (optional adapter)
  exposes a host's ToolCatalog AS an MCP server over streamable HTTP

packages/client                @agentkit/client (optional adapter)
  the other end of that surface: typed REST v1 + SSE client, auto-resuming
  run streams, derived run phases; depends on contracts alone

packages/react                 @agentkit/react (optional adapter)
  headless React hooks over that client: optimistic submit, streamed
  deltas, branch switching, proposals, providers; react is a peer
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
  Mocks, fixtures, golden traces, store-conformance suite,
  seeded concurrent-durability invariant suite.
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
request/response DTOs an HTTP adapter serializes, as types and schemas
only — the DTOs are projections of the host records with the orchestrator's
internals removed (see
[`docs/contracts.md`](contracts.md#rest-v1-surface)). `@agentkit/transport-http`
is the adapter that serves them (see below); this package stays the shape,
not the server.

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
  chat-only / empty-response recovery passes (`turn/retry.ts`). Before
  handing history to a pass, `assembleMessages` reconciles any tool call a
  prior crash left unanswered — `turn/history-reconcile.ts`'s
  `reconcileOrphanToolCalls` synthesizes an in-memory `tool_result_missing`
  failure for it, in-memory only, so the balanced-history invariant holds
  across a restart the same way it holds within one run.
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
embedding host or by the reference adapter packages
(`@agentkit/adapters-memory`, `@agentkit/adapters-sqlite`).

### Optional adapters beside host

Five packages depend on `@agentkit/host` (or, for `client`/`react`, on
`@agentkit/contracts` alone) without `host` depending on any of them — the
same relationship the reference adapters have, and a host is always free to
skip any of them and write the equivalent itself:

- **`@agentkit/mcp-client`** ([`packages/mcp-client/`](../packages/mcp-client))
  — `McpClientManager` plus `createMcpToolSetContributor`, bridging Model
  Context Protocol servers' tools into a run as an ordinary
  `ToolSetContributor`. See [`packages/mcp-client/README.md`](../packages/mcp-client/README.md)
  and [ADR 0004](adr/0004-mcp-client.md).
- **`@agentkit/transport-http`** ([`packages/transport-http/`](../packages/transport-http))
  — `createRestHandler`/`serveRest`, a fetch-standard, zero-dependency
  adapter serving `packages/contracts/src/rest.ts`'s REST v1 surface (HTTP
  + SSE) over any host that implements the port catalog below. See
  [`packages/transport-http/README.md`](../packages/transport-http/README.md)
  and [ADR 0005](adr/0005-http-transport.md).
- **`@agentkit/mcp-server`** ([`packages/mcp-server/`](../packages/mcp-server))
  — the inverse direction from `mcp-client`: exposes a host's `ToolCatalog`
  ([ADR 0011](adr/0011-tool-governance.md)) **as** an MCP server over
  streamable HTTP, so an external MCP client (an IDE, another agent) can
  drive this host's tools. Constant-time bearer auth, a DNS-rebinding
  Host/Origin guard, sessions keyed on a server-minted id bound to the
  opening principal, write-tool filtering on both `tools/list` and
  `tools/call`. See [`packages/mcp-server/README.md`](../packages/mcp-server/README.md)
  and [ADR 0013](adr/0013-serving-surfaces.md).
- **`@agentkit/client`** ([`packages/client/`](../packages/client)) —
  `createAgentKitClient`, the calling end of that same surface: one typed
  method per `REST_ROUTES` operation, `streamRun` as an async iterable that
  resumes on `Last-Event-ID` after a dropped connection, `runPhase` as the
  UI-facing derivation over status plus event log. It sits BESIDE
  `transport-http` rather than below it — it depends on `@agentkit/contracts`
  and nothing else, uses no Node built-in, and runs in a browser. See
  [`packages/client/README.md`](../packages/client/README.md).
- **`@agentkit/react`** ([`packages/react/`](../packages/react)) — headless
  hooks over that client: `useChat` (optimistic pair, streamed deltas
  applied by the same rule the host's own projector uses, then a
  `listMessages` reconcile at the terminal event), `useRun`, `useBranches`,
  `useProposals`, `useProviders`, and one provider component carrying the
  client plus a dependency-free invalidation bus. It sits BESIDE `client`
  for the same reason `client` sits beside `transport-http`: no component,
  no styling, no query cache, `react` as a peer dependency only. See
  [`packages/react/README.md`](../packages/react/README.md).

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
4. **The durable log is canonical.** `RunProjector.project` — what
   `TurnRunner` and any [custom turn executor](#custom-turn-executors) both
   drive ([`packages/host/src/turn/projection.ts`](../packages/host/src/turn/projection.ts))
   — appends to the task's log *before* reflecting the event into
   conversation state, so a projection failure can never erase what happened. Publishing to a live
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
  queued            ──▶ failed        (dependency cascade only — see below)

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
such a host does not have to fork the table. `queued → failed` exists for
exactly one caller — the dependency cascade in `claimNext` (see [Task
dependencies and subagents](#task-dependencies-and-subagents) below) — a task
that never started still has to be able to end `failed` when what it
depended on can never complete.

`AttemptStatus` is `running → completed | failed | abandoned | cancelled`.
`abandoned` is specifically the crash outcome: a lease expired while the
attempt was still `running`, so recovery ends it that way rather than
guessing success or failure.

**Recovery**, as implemented by the reference `SingleProcessTaskRunner`
([`packages/runner-local/src/single-process-task-runner.ts`](../packages/runner-local/src/single-process-task-runner.ts)):

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

**Fencing is enforced on the TERMINAL writes too**, not only on
`appendEvents`/`updateProgress`: `transitionTask`, `endAttempt` and
`markDeadLettered` take an optional `leaseToken`, and a store verifies it
inside the same transaction as the write (`LeaseLostError` otherwise). Without
it a zombie attempt — one whose lease expired mid-tool-call, with recovery
already running attempt 2 — landed the task and ended its own attempt anyway,
burying the live attempt's verdict; a runner cannot close that from outside,
because its `renewLease` pre-check and the write it guards are separated by
awaits. The option is optional so the paths that have no token by construction
still work: recovery acts on a lease it has just deleted, and a cancel from an
HTTP handler never had one. `TurnRunner` orders its terminal block around the
fence — fenced `transitionTask` → `endAttempt` → then the placeholder
`updateMessage` — because `ConversationStore` is lease-unaware and ordering is
the only thing keeping a fenced-out attempt off the live answer (see [ADR
0014](adr/0014-hardening-tranche-2.md)).
`SingleProcessTaskRunner` settles in the same order, for a second reason: an
attempt row closed under a task still `running` with a live lease is exactly
what recovery reads as a crash, and ending the attempt first left that state
behind whenever the transition threw. It cannot be misread now anyway —
`TaskStore.endAttempt` keeps an attempt's FIRST terminal status and writes
nothing on a second call — but the order is what stops the pair from existing.
A renewal is refused once the lease has expired, since the runner asks
`renewLease` *as* its "may I still write?" probe.

*Consequence for consumers*: the task reaches its terminal status a moment
BEFORE the placeholder is finalized, so a client that polls `getTask` and reads
the message in the same breath can catch `placeholder: true` with empty content.
The run event log is the authority on what the answer is — the placeholder is a
projection of it — and the ordering is deliberate: a fenced-out attempt must be
refused before it can touch a message no store can fence.

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
  mine to run?". `ctx.spawnChild?.(input)` — present only when the
  dispatching worker was built with a `TaskService` — submits a child task
  with `parentTaskId` preset to the executing task, so a fanning-out
  executor cannot forge or omit its own lineage. See [Task dependencies and
  subagents](#task-dependencies-and-subagents).
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
  [`docs/ports.md`](ports.md#assistantstore-aggregate)). `cancelTask(taskId)`
  is a breadth-first cascade over `parentTaskId` lineage: a still-`queued`
  descendant is CAS-cancelled directly in the store, a `running`/
  `waiting_approval` one is asked to stop via `taskRunner.requestCancel`
  (cooperative — never forced terminal). See [Task dependencies and
  subagents](#task-dependencies-and-subagents).
- **`createTaskEventWriter`** — see item 5 of [Event flow](#event-flow)
  above.
- **`CHAT_TURN_TASK_KIND`**
  ([`packages/host/src/tasks/kinds.ts`](../packages/host/src/tasks/kinds.ts))
  — `"chat.turn"`, the kind `TurnRunner.submitMessage` creates and
  `ChatTurnExecutor` runs. The `chat.*` and `agentkit.*` prefixes are
  reserved for the framework; everything else belongs to the host.

## Custom turn executors

`chat.turn` is not the only way to answer a conversation. A host whose turn
does not come from `runChat` at all — a chat delegated to a server that
streams its own frames, a replay of a recorded run, a bridge to a provider
SDK this package has no client for — registers **its own kind** and drives
the **same projection**, so the conversation it leaves behind is the one
`chat.turn` would have left rather than a second implementation of the same
rules.

Two additive seams, and nothing else changes:

- **`SubmitMessageInput.kind` / `RegenerateMessageInput.kind`**
  ([`packages/host/src/turn/turn-runner.ts`](../packages/host/src/turn/turn-runner.ts))
  — the task kind the submit creates, defaulting to `CHAT_TURN_TASK_KIND`.
  Everything else about the submit is identical: the same user message and
  empty placeholder in the same single transaction, the same idempotency per
  caller-supplied `taskId`, the same `parentMessageId` branch mechanics.
  **An unknown kind is not validated here** — whether a kind has an executor
  is a deployment fact, and a check at submit time would either refuse a kind
  whose executor lives in another process or pass one nobody registered
  anywhere. The dispatcher answers it at claim time with
  `ExecutorNotFoundError` (terminal, never a dead-letter).
- **`createRunProjector(deps)`**
  ([`packages/host/src/turn/projection.ts`](../packages/host/src/turn/projection.ts))
  — the event → conversation projection, extracted from `TurnRunner` whole.
  `createState({ chatId, assistantMessageId, providerId? })` opens a run;
  `project(ctx, state, event)` appends one **already-stamped** `AiRunEvent`
  to the durable log and then reflects it — deltas onto the placeholder,
  `run.message.completed` with tool calls into an internal assistant record,
  tool results into `role: "tool"` records, `run.usage` into
  `UsageAuthorizer.record`. Every message it writes is a **chain append** off
  `state.lastMessageId` with `activate: false`, so a user switching branches
  mid-run cannot migrate half the run's records onto a conversation that
  never ran them. `reflect(ctx, state, event)` is the same projection without
  the append, for a caller that put the event on the log itself.
  `createRunEventFeed({ projector, ctx, state, tasks, clock, ids })` is the
  drafts-in convenience: it stamps through `createTaskEventWriter` (so the
  numbering has exactly one implementation) and reflects, one append per
  event.

Events arrive **stamped** because on the `chat.turn` path core's
`createEventStamper` owns the numbering for a pass — it was handed a
`firstSeq` and counts upward in memory — and a projector that re-numbered
from `TaskStore.nextSeq` would interleave two counters into one log. The
producer numbers; the projector appends verbatim. A host with no stamper of
its own uses the feed instead, never both for one event.

**What the host still owns**, deliberately: producing the events; finalizing
the placeholder (`content` + `placeholder: false`) when the turn ends; and
the task's terminal transition plus `endAttempt`. Those are decisions about
the run, not about an event — an executor that wanted a different terminal
(a delegated turn landing `waiting_approval`) would otherwise have to fight
a projector that had already settled it.

```ts
class CloudChatExecutor implements TaskExecutor {
  readonly kind = "assistant.cloud-chat";
  constructor(private readonly projector: RunProjector, ...) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    const { chatId, assistantMessageId } = ctx.task.payload as TurnPayload;
    const state = this.projector.createState({ chatId, assistantMessageId });
    const stamp = createEventStamper({
      firstSeq: await store.tasks.nextSeq(ctx.task.taskId),
      attemptId: ctx.attemptId,
    });
    for await (const frame of remote.stream(ctx.task, ctx.signal)) {
      await this.projector.project(ctx, state, stamp(toRunEvent(frame)));
    }
    // The host's half: finalize the placeholder, settle the task.
  }
}
```

## Task dependencies and subagents

`TaskRecord` carries two edges beyond `kind`/`payload`
([`packages/host/src/ports/task-store.ts`](../packages/host/src/ports/task-store.ts)),
deliberately distinct:

- **`parentTaskId` — lineage.** Set by `TaskExecutionContext.spawnChild`
  (never by hand), never a dependency: a child runs the moment the queue can
  claim it, whether or not its parent is still running. Answers "what did
  this task set off?" (`TaskStore.listChildren`, one level) and drives
  `TaskService.cancelTask`'s cascade.
- **`dependsOn` — the claim gate.** Task ids that must reach `completed`
  before this task may be claimed. Immutable after create; every id must
  already exist when the dependent is written
  (`UnknownDependencyError` otherwise), which is what makes the graph a DAG
  **by construction** — an edge can only ever point backward in creation
  order, so no write order can produce a cycle.

`TaskStore.claimNext` enforces the gate — dependency state is queue
semantics, not orchestration. `evaluateTaskDependencies`, exported beside
`assertTaskTransition` for the same reason (every adapter must reach the
same verdict from the same facts), reduces a task's dependency states to one
of three outcomes: `ready` (claim it), `blocked` (skip, try again on a later
claim), or `settle` — a dependency that failed or was dead-lettered settles
the dependent `failed` (`error: "dependency_failed: <id>"`, via the
`queued → failed` edge above), a cancelled one settles it `cancelled` (no
`error` — a cancellation is not a failure). Settlement happens lazily, on
the claim path: nothing is ever re-enqueued, there is no background reaper,
and a chain of dependents resolves over successive claim calls.

`TaskRecord.progress` is a mutable, overwritten snapshot
(`TaskStore.updateProgress`, lease-gated like `appendEvents`) — deliberately
**not** an event: only the latest value matters, and the durable log is not
where a heartbeat percentage belongs.

Full rationale, including the alternatives rejected (eager cascade, claim-time
cycle detection, `dependsOn`-as-re-enqueue): [ADR
0003](adr/0003-task-dependencies-and-subagents.md).

## Conversation branching and fork

A chat is a **tree**, not a list: `MessageRecord` carries `parentMessageId`,
`depth`, `branchIndex`, and a per-message `active` flag that *is* the whole
active-path representation — no materialized path, no walk to read "the
conversation". `ConversationStore.appendMessage`'s optional
`parentMessageId` turns a normal append into a new branch, created active
and switched in with the same write; `activate: false` (with a parent) is the
opposite — a **chain append** that inherits the parent's flag and moves
nothing, which is how a run keeps every record of one turn on the branch that
turn is running against even if the user switches away mid-generation;
`activatePath` moves the active path atomically and answers with it; `forkChat`
copies the active-path prefix up to a message into a brand-new chat,
transactionally, stripping task linkage and re-ordering the copy into
provider order (the fork loses `runId`, so it cannot repair that ordering
later). A chat nobody has branched behaves exactly as it did before this
existed. `TurnRunner`'s history assembly always reads the active path, so
branching and forking are invisible to everything downstream of
`assembleMessages`; branch execution stays serialized per chat (`scopeId` is
unchanged by branching). Full
mechanics, invariants, and the tree operations themselves:
[`docs/ports.md`](ports.md#conversationstore) (authoritative) and [ADR
0007](adr/0007-conversation-branching-fork.md).

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

## A run is not one pass

`TurnRunner` may drive `runChat()` several times under ONE task id, and each
pass writes its own `run.started` … terminal pair onto the same log. The
recovery passes are the chat-only retry (a provider that rejects a request with
`tools` attached) and the empty-response retry (a turn that completed with no
content and no tool calls); the correction harness adds one pass per round (see
[ADR 0012](adr/0012-multi-pass-correction-harness.md)). Why this is a contract
rule and not a client detail — every serving surface had independently assumed
the first terminal event ended the run — is [ADR
0014](adr/0014-hardening-tranche-2.md).

**Every pass after the first is announced first**, with a
`run.warning { code: "retry_pass", pass, reason }` event written immediately
before it runs — `pass` is the 1-based number of the pass about to start,
`reason` is `"chat_only" | "empty_response" | "correction"`. A consumer treats
that event as "the run is live again": the terminal event it just saw belonged
to the previous pass, and the text it has streamed so far must be DROPPED,
mirroring the host's own reset of the stored placeholder. Without the boundary,
a turn whose pass 1 failed and whose pass 2 completed reads as failed, and a UI
concatenating deltas shows pass 1's half-sentence glued to pass 2's answer.

Everything downstream is built on that rule: `transport-http` closes an SSE
stream only when the TASK is terminal (not on the first terminal run event),
`client`'s run phase folds to the LAST terminal, and `useChat`/`useRun` reset
streamed text on the boundary.

## A run is not one attempt, either

A worker that dies mid-turn leaves the task `running` with a live placeholder;
recovery ends the abandoned attempt and starts a new one IN PLACE — same task
id, same event log, one more attempt. Two things make attempt 2 land correctly:

- **It continues attempt 1's chain.** `runTurn` seeds
  `RunProjectionState.lastMessageId` from
  `ConversationStore.lastMessageOfRun(chatId, runId)` — the deepest record this
  run has written — instead of from the placeholder. By attempt 2 the
  placeholder already HAS an active child (attempt 1's internal assistant
  record), and a chain append under a parent that already has an active child
  lands `active: false`; seeding from the placeholder therefore wrote attempt
  2's whole turn onto a dead branch, leaving the conversation replaying attempt
  1's unanswered tool calls forever.
- **Terminal writes are fenced.** The task transition carries the attempt's
  `leaseToken` and goes FIRST, before `endAttempt` and before the placeholder is
  finalized, so an attempt that lost its lease cannot overwrite the live one's
  answer. A `LeaseLostError` stops the rest of the block, on the success path
  and the failure path alike. See [ADR
  0014](adr/0014-hardening-tranche-2.md).

**An unexpected throw is bookkept in full.** `TurnRunner.executeTask` records a
terminal `run.failed` (or `run.cancelled` when the run was aborted) on the
durable log, lands the task fenced, and finalizes the placeholder
(`placeholder: false`, keeping whatever streamed) — in that order, all
best-effort. Only the task transition used to happen, which left an SSE consumer
watching the stream stop with no terminal event and a UI spinning on a message
nothing was coming back to finish.

## One live turn per chat

`submitMessage` and `regenerate` create their task with
`CreateTaskInput.exclusiveScope`, so a submit into a chat that already holds an
unfinished task is refused with `ChatBusyError` (`chat_busy`, HTTP 409) — by the
STORE, in the same transaction that would have written the user message (why it
is on by default: [ADR 0014](adr/0014-hardening-tranche-2.md)). A
second concurrent turn does not work: its user message takes the active-leaf
slot under the live run's internal records, and the live run's next chain append
then lands off the path (the same rule as above). A redelivered `taskId` is
still answered as a duplicate, not refused as busy, so idempotent callers are
unaffected. Hosts that queue turns deliberately opt out with
`TurnRunnerDeps.allowConcurrentSubmit`.

## Host hooks run under deadlines

`ContextProvider`, `AttachmentResolver`, `ToolSetContributor.contribute` and
`VerificationHook` are all host code the framework awaits inside a leased
attempt. Each runs under a deadline from `TurnRunnerDeps.hookTimeoutsMs`
(defaults: verify 30 s, context 10 s, attachments 10 s, contribute 15 s; a
non-positive value turns one off — [ADR
0014](adr/0014-hardening-tranche-2.md)). A deadline is a RACE, not a cancellation —
nothing can stop host code that is not watching a signal, so a late answer is
discarded — and every one of them degrades rather than failing the turn:

| Hook | On timeout | On the log |
|---|---|---|
| `context.*` | no bindings / no system prompt | `run.warning hook_timeout` |
| `contribute` | that contributor's tools are missing; the others stage | `run.warning hook_timeout` |
| `attachments.resolve` | the image part is dropped from the pass | `run.warning attachment_unresolved` |
| `verify` (harness) | `"unavailable"`, harness stops | `run.verification` |
| `verify` (single-shot) | the turn fails — unchanged semantics, now bounded | `run.failed` |

## Streaming writes are coalesced

Every `run.message.delta` is appended to the durable log, in order, before
anything else happens — that is unchanged and is what a consumer follows. The
PLACEHOLDER behind it is a projection of that log, so its `updateMessage` is
throttled to at most one per 32 deltas or 50 ms, always flushed before any
non-delta event (`run.message.completed` and every terminal included) and
discarded by a pass reset. A 2000-delta answer costs 63 row writes instead of
2000 (`scripts/bench-projection.ts`: 201.8 ms → 5.2 ms on sqlite; [ADR
0014](adr/0014-hardening-tranche-2.md)). What a crash
can cost is under 50 ms of half-written text in a record the next attempt
overwrites anyway.

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
