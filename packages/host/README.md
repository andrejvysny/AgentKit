# @agentkit/host

Durable orchestration layer over `@agentkit/core`'s pure run loop: storage
ports, task/proposal state models, kind-dispatched task execution, the
proposal service, and `TurnRunner` — the `TaskWorker` (and `chat.turn`
`TaskExecutor`) that drives `runChat()` durably.

This package implements no storage or execution itself. It defines the
ports (`packages/host/src/ports/`) an embedding host implements, plus the
services built on top of them (`TurnRunner`, `ProposalService`,
`SessionWritePolicy`). See [`docs/ports.md`](../../docs/ports.md) for the
full port catalog and [`docs/architecture.md`](../../docs/architecture.md)
for how this layer fits between `@agentkit/core` and a storage backend.

Two complete reference implementations of every port exist in
`internal/reference-adapters` (workspace-private, not published) — read
that package before writing your own adapter from scratch.

## Modules

- `ports/` — the port catalog: `AssistantStore` (aggregate),
  `ConversationStore`, `TaskStore` (`parentTaskId`/`dependsOn` edges,
  dependency-gated `claimNext`, `listChildren`, `updateProgress` —
  see [ADR 0003](../../docs/adr/0003-task-dependencies-and-subagents.md)),
  `ProposalStore`, `ProviderStore`,
  `SettingsStore`, `OutboxStore`, `TaskRunner`/`TaskWorker`, `WritePolicy`,
  `ProposalApplier`, `VerificationHook`, `ContextProvider`,
  `ToolSetContributor`, `SecretStore`, `AuthorizationPort`,
  `UsageAuthorizer`, `system.ts` (`Clock`/`IdGenerator`/`Logger`).
- `tasks/` — the kind-dispatch layer over `TaskStore`/`TaskRunner`:
  `kinds.ts` (`CHAT_TURN_TASK_KIND`; `chat.*`/`agentkit.*` prefixes
  reserved), `task-executor.ts` (`TaskExecutor`, `TaskExecutionContext` —
  including `spawnChild`, which presets `parentTaskId` to the spawning
  task so lineage cannot be forged), `executor-registry.ts`
  (`ExecutorRegistry`, `createDispatchingWorker` —
  an unregistered kind is a terminal failure, never a dead-letter),
  `task-service.ts` (`TaskService`: `createTask`/`dispatch`/`submitTask`,
  dispatch strictly post-commit, idempotent resubmit per caller-supplied
  `taskId`; `cancelTask` — a cooperative, breadth-first cascade over
  `parentTaskId` lineage), `task-event-writer.ts` (`createTaskEventWriter`
  — stamps and appends host-side/non-chat task events; barred from inside
  a chat pass, where core's stamper owns numbering). See
  [`docs/architecture.md`](../../docs/architecture.md#task-kinds-and-executors).
- `proposals/` — `state-machine.ts` (`PROPOSAL_TRANSITIONS`),
  `proposal-service.ts` (`ProposalService`: stage/approve/reject/apply/
  reconcileInterrupted), `action-id.ts` (model-supplied idempotency-key
  parsing), `proposal-builder-tool.ts` (`createProposalBuilderTool` — the
  wrapper a host's write tools are built on).
- `policy/session-write-policy.ts` — `SessionWritePolicy`, an in-memory
  `WritePolicy`.
- `turn/` — `turn-runner.ts` (`TurnRunner`, `ChatTurnExecutor`), `retry.ts`
  (chat-only / empty-response retry decisions), `message-order.ts`
  (`orderMessagesForProvider`), `emulated-tool-call.ts`
  (`looksLikeEmulatedToolCall`), `registry-staging.ts` (`stageRegistry`),
  `history-reconcile.ts` (`reconcileOrphanToolCalls` — synthesizes an
  in-memory `tool_result_missing` failure for a persisted tool call whose
  result was lost to a crash between `projectEvent`'s separate writes;
  called from `assembleMessages`, never persisted).
- `bootstrap.ts` — `recoverOnBoot({ taskRunner, proposals })`: the startup
  pass that cleans up after a crash — `TaskRunner.recover()` first, then
  `ProposalService.reconcileInterrupted()` — before any worker claims work.
- `errors.ts` — `AgentKitHostError` and its subclasses, each carrying a
  stable machine-readable `code` (`invalid_task_transition`,
  `duplicate_task`, `executor_not_found`, `lease_lost`, `seq_conflict`,
  `duplicate_action_id`, `revision_conflict`, `not_found`,
  `invalid_proposal_transition`, `unknown_dependency`).

## Embedding `TurnRunner`

`TurnRunner` implements `TaskWorker`; a `TaskRunner` (your own, or the
reference `SingleProcessTaskRunner`) claims `chat.turn` tasks and calls
`turnRunner.execute(execution)`. The full wiring, exercised end-to-end
against real (non-mocked) `TurnRunner` + `SingleProcessTaskRunner` +
`MemoryAssistantStore` code, with only the provider faked, lives at
[`internal/reference-adapters/tests/task-runner-integration.test.ts`](../../internal/reference-adapters/tests/task-runner-integration.test.ts).
The sketch:

```ts
import {
  TurnRunner,
  defaultClock,
  defaultIds,
  recoverOnBoot,
} from "@agentkit/host";
import { MemoryAssistantStore, SingleProcessTaskRunner } from "@agentkit/reference-adapters";

const store = new MemoryAssistantStore();
const taskRunner = new SingleProcessTaskRunner({ store });

const turnRunner = new TurnRunner({
  store,
  taskRunner,
  providerFactory: (config) => buildYourProviderClient(config),
  contributors: [], // ToolSetContributor[]
  clock: defaultClock,
  ids: defaultIds,
});

// Clean up after the last crash BEFORE claiming anything: expired leases and
// abandoned attempts, then the proposals a dying process left mid-apply.
await recoverOnBoot({ taskRunner, proposals: proposalService });

const handle = await taskRunner.startWorker(turnRunner, { concurrency: 2 });

// Never awaits the model — the run is durable the instant this returns.
const { runId, assistantMessageId } = await turnRunner.submitMessage({
  chatId: "chat-1",
  content: "Hi",
});

// ... elsewhere: subscribe to store.tasks.listEvents(runId) / the outbox
// to stream the answer as it lands.

await handle.stop();
```

`submitMessage` writes the user message, an empty assistant placeholder, and
a `queued` task of kind `chat.turn` in one transaction, then enqueues and
returns immediately — so a crash a millisecond later loses nothing.
`execute` is what the queue calls back; it drives `runChat()`, appends every
event to the durable log, and projects it into conversation state. Retries
stay inside one task id (its value is `SubmitMessageResult.runId`): each
pass reads `TaskStore.nextSeq(taskId)` as its `firstSeq`, so a consumer
reconnecting mid-retry still sees one unbroken sequence (see
[`docs/architecture.md`](../../docs/architecture.md#event-flow)).

## Port implementation checklist

Implementing `AssistantStore` for your own backend (Postgres, Redis, a
managed service):

1. **Start from a reference adapter.** `internal/reference-adapters/src/`
   has a complete `MemoryAssistantStore` and `SqliteAssistantStore` — both
   pass the conformance suite below, and are the shortest path to seeing
   every port method implemented once.
2. **Run `describeAssistantStoreConformance`** from `@agentkit/testing`
   against your adapter as you build it, not after. It asserts the
   invariants ports document but cannot enforce by type alone: transition
   legality (CAS semantics, illegal-edge rejection), `(scopeKey, actionId)`
   uniqueness with the release-on-rejected/invalidated exception, `seq`
   monotonicity and lease-token rejection in `appendEvents`, atomic
   `claimNext` under a busy scope, outcome idempotency on `operationId`.
3. **Get fencing right first.** `TaskStore.acquireLease` must hand out a
   strictly higher `fencingToken` than any lease ever issued for that task,
   even across restarts — the reference sqlite adapter draws it from a
   single-row monotonic counter table (`fencing_counter`); a Postgres
   adapter would use a sequence.
4. **Decide your transaction story.** If your backend cannot roll back
   (e.g., a plain key-value store with no multi-key transaction), your
   `create()` factory for the conformance suite must report
   `capabilities: { atomicTransactions: false }` — see
   `MemoryAssistantStore`'s doc comment for why that is an honest
   limitation, not a suite it fails.
5. **Wire a `TaskRunner`.** `SingleProcessTaskRunner` is single-process by
   design (see [`docs/non-goals.md`](../../docs/non-goals.md)) — a
   distributed deployment needs its own claim/lease/recover loop over your
   `TaskStore`, but can reuse `error-classifier.ts`'s retry-classification
   logic and `ScopeLock`'s per-process serialization approach as a model.
6. **Implement the remaining ports as your host needs them.** `WritePolicy`,
   `ProposalApplier`, `VerificationHook`, `ContextProvider`,
   `ToolSetContributor`, `SecretStore`, `AuthorizationPort`, and
   `UsageAuthorizer` are all independent of your storage choice — see
   [`docs/ports.md`](../../docs/ports.md) for each one's responsibility and
   key invariant.

## License

MIT
