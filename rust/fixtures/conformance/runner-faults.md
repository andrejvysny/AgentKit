# Executing-runner fault conformance

`runner-faults-v1.json` defines six deterministic fault profiles, clock boundaries,
and reviewed observable checkpoints. Both drivers execute the actual runtimes:
TypeScript `SingleProcessTaskRunner` with its real memory/SQLite adapters, and
native `AgentHost` with real memory/file SQLite. Expected observations are authored
in `scripts/tests/rust-runner-conformance-model.ts`; they are not produced by a
second runner model. A drift test reconstructs the checked-in fixture.

## Observations

Each checkpoint compares task status, task attempt count, ordered attempt
statuses, committed effects, event sequence/type/data, poison count, dead-letter
presence, and terminal failure classification. Verification records the durable
task status and event types seen before settlement. Retry checkpoints also expose
the scheduled deadline and whether it is persisted. The suite validates event
contract version, event/attempt identity presence, and timestamp syntax before
removing opaque identities and timestamps from the comparison. It does not
renumber, reorder, or discard events.

The TS ports have no public attempt-history read operation. For memory and
`:memory:` SQLite, a transparent observer records actual `claimNext`,
`createAttempt`, and `endAttempt` return values; all calls still delegate to the
real adapter. Reopened file histories are read directly from SQLite without
mutation. Native histories use `SqliteStore::attempts`.

## Fault profiles

| Profile | Executed boundary |
| --- | --- |
| Safe retry | Explicit pre-dispatch failure, exact 200 ms backoff, no dispatch at 1199 ms, next attempt at 1200 ms. |
| Uncertain write | Commit an effect and event, then throw raw `ECONNRESET`; count any replay. |
| Cancellation after commit | Hold result publication after the effect, cancel, then publish committed evidence under the still-current fence before cancellation settles. |
| Stale publication | Replace the lease while an actual execution is suspended; reject publication from its captured context and deny its terminal verdict. The newer lease leaves the task running. |
| Verification | Read durable committed evidence while the task is running, append `run.verification`, then settle. |
| Crash/recovery | Kill a real child process during safe backoff, reopen at lease expiry, manually recover, reopen again, recover twice, then explicitly resume. |

The native stale-context probe runs explicitly before releasing the suspended
worker because its heartbeat may detect the replacement first and drop the
executor future. Either path refuses old ownership; the explicit probe also
proves the publication API rejects the captured old fence.

## Deliberate reference differences

Two profiles have separate, exact expected traces. These differences remain
visible; they are not normalized into equality.

1. Frozen TS stores retry timing in runner memory and keeps a live lease during
   backoff. Native persists retry intent and deadline before releasing the failed
   attempt's lease. Both dispatch at the same deadline. The native change prevents
   restart from losing the retry decision.
2. Frozen TS treats raw `ECONNRESET` after a committed effect as transient and
   repeats the effect. Native requires explicit safe-to-retry evidence. It settles
   `failed` with `uncertain_outcome` after one effect. This is the approved safety
   correction against automatic replay of an uncertain external write.

## Limits

The effects are deterministic executor callbacks and counters, not live provider,
MCP, or domain-service requests. Cancellation exercises the executor's asynchronous
effect/result boundary rather than the full chat tool pipeline. Native uses its
real `VerificationHook`; TS performs the final verification callback in its actual
generic worker, which has no equivalent generic runner hook. This establishes the
observed event-before-settlement ordering, not complete chat verification parity.

Process termination is tested on file stores at a pre-dispatch safe-failure
boundary. The suite does not prove recovery after an external write committed but
before its receipt was recorded. Recovery is manual; automatic redispatch,
arbitrary failure schedules, seeded fuzzing, provider-level retry protocols,
distributed workers, and every heartbeat race remain outside these six profiles.

## Run

```sh
bun test scripts/tests/rust-runner-conformance.test.ts
cargo test --manifest-path rust/Cargo.toml -p agentkit-host --test runner_conformance --locked --offline
AGENTKIT_RUN_RUST_RUNNER_CONFORMANCE=1 bun test scripts/tests/rust-runner-conformance.test.ts
```

The enabled differential gate runs the native test binary, including its killed
child process, writes an actual native JSON report to a temporary path, and
compares it against fresh actual TS execution. Common profiles must match each
other and their reviewed expectations exactly. Approved differences must match
their separate expectations exactly and must remain observably different.
