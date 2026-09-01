# @agentkit/runner-local

**A complete `TaskRunner` for ONE process: claim, execute, heartbeat,
classified retry with exponential backoff, dead-letter, recover.**

It owns no state a restart needs. Everything that decides what happens next —
task status, attempt history, lease ownership, the fencing token — lives in the
`AssistantStore` it is handed. Kill the process mid-task and the next one's
`recover()` finds the expired lease and continues the SAME task, on a new
attempt, with the event sequence unbroken.

```ts
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";

const store = new SqliteAssistantStore("./data/agentkit.sqlite");
const runner = new SingleProcessTaskRunner({ store });

await runner.recover(); // before anything starts claiming
const handle = await runner.startWorker(worker, { concurrency: 4 });
```

**That order matters, and it works.** `recover()` runs with no worker to hand
work to, and its expiry pass has already deleted the lease by the time it finds
that out — so a task it cannot dispatch is parked internally and re-dispatched
the instant `startWorker` runs. Dropping it would strand the task `running`
with no lease: unclaimable (the loop only takes `queued`) and invisible to
every later `recover()` (which only sees expired leases).

Any store that passes `@agentkit/testing`'s
`describeAssistantStoreConformance` drives it —
[`@agentkit/adapters-memory`](../adapters-memory),
[`@agentkit/adapters-sqlite`](../adapters-sqlite), or a host's own.

## What's here

- **`SingleProcessTaskRunner`** — the runner. Dispatch is **fire-and-forget**:
  the claim loop never awaits an execution, so `concurrency: N` actually runs N
  attempts at once. Retry happens **in place** — a task that started stays
  `running` for its whole life and gets a new attempt (new lease, new fencing
  token) rather than going back to `queued`, because the `TaskStore` transition
  table has no `running → queued` edge and pretending otherwise would make the
  task claimable by a second worker while the first is still landing.
- **`classifyExecutionError`** — transient / terminal / cancelled, decided from
  structured signals (a host error `code`, an explicit `retryable` flag, an
  HTTP status) before falling back to message heuristics. **Unrecognized
  failures are terminal**, not retried forever: a bad API key should fail once,
  not three times.
- **`ScopeLock`** — in-memory, per-process serialization so two tasks sharing a
  scope (usually a chat id) never execute concurrently, plus the queue
  positions a UI wants to render ("you are second in line for this chat"). A
  dispatch optimization only: correctness rests on the store's `claimNext` +
  leases, never on this lock.

## Retry backoff

A transient failure waits before its next attempt:
`min(baseMs * 2^(attemptCount - 1), maxMs)`, spread by `± jitterRatio`.
Defaults: 1s base, 30s ceiling, 0.2 jitter — configurable per runner:

```ts
new SingleProcessTaskRunner({
  store,
  maxAttempts: 3,
  retryBackoff: { baseMs: 1_000, maxMs: 30_000, jitterRatio: 0.2 },
});
```

Without it, a retry started on the very next poll cycle, which turns the one
failure mode retries exist for — a provider that is briefly unavailable — into
three requests in ~200ms, all arriving while it is still down. The jitter is
what keeps N tasks that failed on the same outage from retrying in lockstep.

The deadline is tracked in the runner, never in the store: a task mid-backoff
is still `running`, still leased by this process and still holding its scope,
so no other claimant can see it and there is nothing for a restart to read — a
crash during the wait is recovered exactly like a crash during the attempt. The
delay is measured against the injected `Clock`, so a test drives it rather than
sleeping. `jitterRatio: 0` makes it exact; `baseMs: 0` turns it off.

Two things the wait is re-checked for before the next attempt starts. `stop()`
arriving mid-backoff leaves the task `running` with its **live lease** — the
lease is deliberately NOT released, because it is the only thing the next
process's `recover()` can find the task by. And if the lease MOVED while the
backoff ran (a heartbeat that could not reach the store, then someone else's
recovery), the wait ends with a fencing check and this runner writes nothing:
minting a fresh lease there would steal back a task that already has an owner
and run it twice, at once.

## Single-process limits

- **Cancellation is cooperative, and in-memory.** `requestCancel` on a running
  task aborts the `AbortController` this process registered for it, and the
  worker decides how to land. A cancel aimed at a task some *other* process is
  executing does nothing here — `recover()` reconciles it once that owner's
  lease expires. A durable, cross-process cancel needs a cancellation flag in
  the store that every worker polls, which is a different design with a
  different cost. There is no way to stop a worker that ignores its signal.
- **`ScopeLock` is per-process.** Two runners in two processes do not share it;
  what keeps them from executing the same task is the store's `claimNext` +
  leases.
- **One worker per runner.** A second would race this one's active map and
  concurrency budget for no benefit — run two runners if two workers are
  wanted.

See [`docs/non-goals.md`](../../docs/non-goals.md) for what a distributed queue
would have to add.

## Graded by the shared contract

`@agentkit/testing`'s `describeTaskRunnerConformance` is the `TaskRunner` port's
behavioral contract — enqueue idempotency, recovery from an expired lease,
lease renewal across an attempt that outlives the TTL, cancellation reaching a
running worker, and the concurrency budget. This
package runs it against **both** reference stores
(`tests/task-runner-conformance.test.ts`), because those promises are all
statements about what ends up in the store and the two stores reach those
states by completely different means.

## License

MIT
