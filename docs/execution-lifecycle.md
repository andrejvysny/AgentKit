# Execution, recovery and public settlement

The host owns execution policy. AgentKit keeps one `runChat` tool loop; the
provider adapter supplies transport events, and `TurnRunner` adds durable
projection, policy, correction and verification.

## Budgets

Set `TurnRunnerDeps.executionBudgets` for the deployment's provider class:

```ts
const executionBudgets = {
  firstByteMs: 30_000,
  streamIdleMs: 30_000,
  toolMs: 60_000,
  overallMs: 300_000,
  providerRequests: 8,
  toolCalls: 24,
  correctionPasses: 2,
  retries: 1,
};
```

These values are illustrative policy, not universal defaults. Omitted limits
remain unbounded. Milliseconds and counters must be nonnegative safe integers.
The overall deadline starts with the original task execution; restarting or
correcting a run does not replenish it. The host checkpoints counters in task
progress before starting external work. A failed checkpoint starts no new work.

`firstByteMs` and `streamIdleMs` measure valid provider activity, including
reasoning and tool-argument frames. Supplied HTTP adapters reserve a provider
request before every transport dispatch, including compatibility retries.
Custom clients that advertise `tracksTransportRequests: true` must await
`AiChatRequest.beforeRequest` before each dispatch. Other clients consume one
request reservation per `streamChat` invocation.

User cancellation, shutdown, lease loss and deadlines stop new provider/tool
work. Late responses cannot publish success. `execution_timeout` and
`execution_budget_exhausted` are failures; user cancellation remains cancelled.
A committed domain operation remains committed: reconcile its durable receipt.
Neither cancellation nor event replay reverses or repeats that operation.

## Recovery

`SingleProcessTaskRunner` defaults to `recoveryMode: "automatic"` and
`shutdownMode: "drain"`, preserving existing deployment behavior. Desktop hosts
that require explicit user action should choose `recoveryMode: "manual"` and
`shutdownMode: "cancel"`.

Run boot recovery before starting a manual worker. It parks pre-existing queued
work and abandoned attempts as `interrupted`; it does not dispatch inference or
new domain mutations. Existing proposal receipt reconciliation may inspect and
record an already committed outcome. A nonexpired lease remains owned until it
expires; recovery must not take another live worker's work.

`TaskService.resumeTask(taskId)`, `TaskRunner.resume(taskId)` and REST
`POST /runs/:runId/resume` retain the original task ID and saved progress. Only
an interrupted task can resume. Cancellation can also settle an interrupted
task without dispatching it. Resume is explicit intent, not permission to
ignore exhausted budgets, stale revisions or operation deduplication.

Hosts should keep exclusive chat admission enabled. `chat_busy` is HTTP 409;
clients retain the server's original active run identity for Stop and reconnect.
An interrupted run remains active until resumed or cancelled.

## Durability and settlement

A provider's `run.completed`, `run.failed` or `run.cancelled` describes a
provider pass. Host correction or verification may follow. Client event folding
therefore reports `settling`; authoritative task status determines the final
completed, incomplete, failed or cancelled result.

The projector commits events before applying their conversation projection.
Stable event-backed message IDs make replay idempotent after a crash. Every
worker-owned message write checks the live, unexpired lease inside its storage
transaction. Final message and attempt updates precede the terminal task update
in the same SQLite transaction. Public task/event/message reads wait for commit.
Memory preserves ownership checks but has no durable storage or rollback.

Custom storage adapters must implement `ConversationStore.getMessage` for
stable projection lookup and honor optional `RunWriteFence` arguments on
message mutations. Manual recovery also requires `TaskStore.interruptQueued`.
These adapter obligations accompany the 0.6 contract change; an adapter that
cannot fence writes must reject them rather than silently ignore ownership.

SSE remains open through host verification until task settlement or manual
interruption, then drains the committed log. Only after that drain does it emit
`event: agentkit.stream.settled` with `data: {}`. This transport control frame
has no event ID or sequence and is not a durable run event. It confirms that
the log was exhausted; it does not mean the task succeeded. Read task status
to distinguish completed, failed, cancelled and interrupted outcomes.

A pump error still rejects the response body. Bun 1.3.14 can serialize that
error as clean HTTP EOF, so EOF alone is never authoritative. `streamRun`
requires the settlement frame and reconnects from its last delivered cursor
when EOF lacks it, within the existing retry budgets. `drainRun` rejects an
unmarked EOF rather than returning a partial tail. Deploy matching client and
transport versions: older servers without this frame fail closed with the new
client. The frame is part of the coordinated, unpublished 0.6/0.7 candidates.
Headless hooks also reconcile task status and final verification.
Unknown cursors replay the retained log, with stable IDs and monotonic sequence
numbers suppressing duplicates. Bounded client buffers may omit old events but
must not refold their replay as new progress.

Unknown submission outcomes retain an immutable body and idempotency key across
hook remounts sharing the same client. Applications that need retries across a
full reload must persist and supply their own idempotency key. Explicit resume
and a new user submission are separate operations.
