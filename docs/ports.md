# Port catalog

`@agentkit/host` defines no storage or execution of its own — it defines
interfaces ("ports") an embedding host implements, plus the record types and
frozen transition tables those interfaces are defined in terms of. Two
complete implementations exist for local development and tests:
`internal/reference-adapters`'s `MemoryAssistantStore` (Map-backed) and
`SqliteAssistantStore` (`bun:sqlite`-backed) — see that package's
[README](../internal/reference-adapters/README.md). Both pass
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
(`conversations`, `runs`, `proposals`, `providers`, `settings`, `outbox`)
plus `transaction<T>(fn)`. Grouped rather than injected separately because
the operations that matter span them — submitting a turn writes a user
message, a placeholder assistant message, and a run row together; finishing
one writes events, a message, and a status together — and those writes must
land as one unit or not at all.

**Key invariant**: `transaction(fn)` either commits every write `fn` makes
or rolls all of it back on a throw. An adapter that cannot roll back (a
plain in-memory store) must declare `capabilities.atomicTransactions: false`
to the conformance harness rather than silently pass a weaker guarantee.

**Reference / conformance**: `MemoryAssistantStore` and
`SqliteAssistantStore` in
[`internal/reference-adapters/src/`](../internal/reference-adapters/src/);
conformance suite in
[`packages/testing/src/store-conformance.ts`](../packages/testing/src/store-conformance.ts).

### `ConversationStore`

[`ports/conversation-store.ts`](../packages/host/src/ports/conversation-store.ts)

Chats and messages. Owns `MessageRecord.orderKey`, the per-chat ordering
key — not `createdAt`, for the same reason `AiRunEvent.seq` orders events:
several messages can be written in one transaction within the same
millisecond.

**Key invariant**: `appendMessage` assigns the next `orderKey` in the store,
not the caller, so concurrent appends from a run and from a user land in a
defined order. `updateMessage`'s `metadata` patch *replaces* the stored bag
rather than merging — a merge would make "unset this flag" unexpressible.

### `RunStore`

[`ports/run-store.ts`](../packages/host/src/ports/run-store.ts)

Durable run lifecycle: status (`RunStatus`, `RUN_TRANSITIONS`), attempts,
leases, and the event log — the product every UI replays and every crash
recovery reads.

**Key invariants**:
- `transitionRun` is compare-and-set: it MUST reject when the run's current
  status is not in the caller's `from` set (someone else moved it first —
  a lost race, not a retryable hiccup), and MUST reject a transition not in
  `RUN_TRANSITIONS`.
- `appendEvents` MUST reject a stale `leaseToken` (`LeaseLostError`) and a
  non-monotonic `seq` (`SeqConflictError`); it MUST NOT re-stamp `seq`
  — core owns numbering.
- `claimNext` MUST be atomic: claiming a run creates its attempt and lease
  in the same operation, so no other caller can claim the same run.

**Reference / conformance**: `MemoryRunStore` / `SqliteRunStore`
(fencing enforced via a guarded `UPDATE ... WHERE lease_token=?` plus the
driver's reported `changes` count, in a transaction); conformance suite
covers CAS rejection, lease renewal/expiry with a strictly higher
`fencingToken` on re-acquire, `seq` monotonicity rejection, and atomic
`claimNext` under a busy scope.

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
settings. `OutboxStore` is the transactional-outbox pattern: a run's events
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

The durable queue that turns "a run exists" into "a worker is executing
it". Deliberately has **no `subscribe()`** — events reach consumers through
the run event log and the outbox, both of which survive a restart; a
subscription on the runner would be a second, lossier channel.

**Key invariants**:
- `enqueue` is idempotent per `runId`: a redelivered enqueue for a run that
  is no longer `queued` is a silent no-op, never a second execution of one
  turn.
- `recover()` is the startup pass: expire dead leases, end their attempts
  `abandoned`, reconcile interrupted applies by operation id, then
  re-enqueue or dead-letter — run before any worker starts claiming.

**Reference**: `SingleProcessTaskRunner`
([`internal/reference-adapters/src/task-runner/single-process-task-runner.ts`](../internal/reference-adapters/src/task-runner/single-process-task-runner.ts)) —
claim/execute/heartbeat/retry/dead-letter/recover for one process, with
fire-and-forget dispatch (never awaits an execution inside its claim loop)
and evidence-based error classification (`error-classifier.ts`: an
unrecognized failure is terminal by default, not blindly retried). Explicitly
single-process: cancellation of a run another process owns is not delivered
— see [`docs/non-goals.md`](non-goals.md).

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

### `ToolSetContributor`

[`ports/tool-contributor.ts`](../packages/host/src/ports/tool-contributor.ts)

A source of tools for a run, contributed per run rather than registered
once at boot — which tools exist depends on what the chat is bound to and
what the user is allowed to do, both of which change between turns.

**Key invariant**: `unboundToolNames()` (optional) declares which of a
contributor's tools stay available when the chat has no primary binding.
`turn/registry-staging.ts`'s `stageRegistry` prunes by this hook alone,
never by a hardcoded list — only the contributor that wrote a tool knows
whether it can operate on nothing, and when *no* contributor declares the
hook, nothing is pruned (an absent declaration means "no opinion", not
"empty the registry").

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

### `UsageAuthorizer`

[`ports/usage-authorizer.ts`](../packages/host/src/ports/usage-authorizer.ts)

Spend control around provider calls: `authorize()` before, `record()`
after. Two methods rather than one because the interesting failure is
between them — a run authorized on an estimate and then far over budget
must still be recorded, so the next authorization can refuse.

## System seams

[`ports/system.ts`](../packages/host/src/ports/system.ts)

`Clock` (`now()`/`nowIso()`), `IdGenerator` (one method per entity kind —
`runId`, `attemptId`, `eventId`, `proposalId`, `operationId`, `messageId`
— so a fake can hand out readable per-kind sequences), and `Logger`
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
