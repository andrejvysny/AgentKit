# @agentkit/host

Durable orchestration layer over `@agentkit/core`'s pure run loop: storage
ports, run/proposal state models, the proposal service, and `TurnRunner` —
the `TaskWorker` that drives `runChat()` durably.

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
  `ConversationStore`, `RunStore`, `ProposalStore`, `ProviderStore`,
  `SettingsStore`, `OutboxStore`, `TaskRunner`/`TaskWorker`, `WritePolicy`,
  `ProposalApplier`, `VerificationHook`, `ContextProvider`,
  `ToolSetContributor`, `SecretStore`, `AuthorizationPort`,
  `UsageAuthorizer`, `system.ts` (`Clock`/`IdGenerator`/`Logger`).
- `proposals/` — `state-machine.ts` (`PROPOSAL_TRANSITIONS`),
  `proposal-service.ts` (`ProposalService`: stage/approve/reject/apply/
  reconcileInterrupted), `action-id.ts` (model-supplied idempotency-key
  parsing), `proposal-builder-tool.ts` (`createProposalBuilderTool` — the
  wrapper a host's write tools are built on).
- `policy/session-write-policy.ts` — `SessionWritePolicy`, an in-memory
  `WritePolicy`.
- `turn/` — `turn-runner.ts` (`TurnRunner`), `retry.ts` (chat-only /
  empty-response retry decisions), `message-order.ts`
  (`orderMessagesForProvider`), `emulated-tool-call.ts`
  (`looksLikeEmulatedToolCall`), `registry-staging.ts` (`stageRegistry`).
- `errors.ts` — `AgentKitHostError` and its subclasses, each carrying a
  stable machine-readable `code` (`invalid_run_transition`, `lease_lost`,
  `seq_conflict`, `duplicate_action_id`, `revision_conflict`, `not_found`,
  `invalid_proposal_transition`).

## Embedding `TurnRunner`

`TurnRunner` implements `TaskWorker`; a `TaskRunner` (your own, or the
reference `SingleProcessTaskRunner`) claims runs and calls
`turnRunner.execute(execution)`. The full wiring, exercised end-to-end
against real (non-mocked) `TurnRunner` + `SingleProcessTaskRunner` +
`MemoryAssistantStore` code, with only the provider faked, lives at
[`internal/reference-adapters/tests/task-runner-integration.test.ts`](../../internal/reference-adapters/tests/task-runner-integration.test.ts).
The sketch:

```ts
import { TurnRunner, defaultClock, defaultIds } from "@agentkit/host";
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

const handle = await taskRunner.startWorker(turnRunner, { concurrency: 2 });

// Never awaits the model — the run is durable the instant this returns.
const { runId, assistantMessageId } = await turnRunner.submitMessage({
  chatId: "chat-1",
  content: "Hi",
});

// ... elsewhere: subscribe to store.runs.listEvents(runId) / the outbox
// to stream the answer as it lands.

await handle.stop();
```

`submitMessage` writes the user message, an empty assistant placeholder, and
a `queued` run in one transaction, then enqueues and returns immediately —
so a crash a millisecond later loses nothing. `execute` is what the queue
calls back; it drives `runChat()`, appends every event to the durable log,
and projects it into conversation state. Retries stay inside one run id:
each pass reads `RunStore.nextSeq(runId)` as its `firstSeq`, so a consumer
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
3. **Get fencing right first.** `RunStore.acquireLease` must hand out a
   strictly higher `fencingToken` than any lease ever issued for that run,
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
   `RunStore`, but can reuse `error-classifier.ts`'s retry-classification
   logic and `ScopeLock`'s per-process serialization approach as a model.
6. **Implement the remaining ports as your host needs them.** `WritePolicy`,
   `ProposalApplier`, `VerificationHook`, `ContextProvider`,
   `ToolSetContributor`, `SecretStore`, `AuthorizationPort`, and
   `UsageAuthorizer` are all independent of your storage choice — see
   [`docs/ports.md`](../../docs/ports.md) for each one's responsibility and
   key invariant.

## License

MIT
