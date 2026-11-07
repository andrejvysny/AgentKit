# Executing-runner fault conformance

`runner-faults-v1.json` defines six deterministic fault profiles, clock boundaries,
and reviewed observable checkpoints, plus the M0 boundary profiles listed under
"M0 boundary profiles". Both drivers execute the actual runtimes:
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

## M0 boundary profiles

Nine further profiles in this schedule close the boundaries the six
differential profiles do not reach, one per M0 fault-matrix edge. They are
authored in this file and executed by the **Rust host only**: each carries an
empty `typescript` expectation list because the frozen
`SingleProcessTaskRunner` has no admission gate, no execution receipt, no
approval record, and no outbox claim lane to observe. They are driven by
`rust/crates/host/tests/m0_fault_*.rs`, which reuse this suite's `Clock`,
`Witness`, `Environment`, `snapshot` and `Script` fixture; a profile missing
from the schedule fails the test that names it rather than being skipped.

| Profile | Executed boundary | Rust test |
| --- | --- | --- |
| `m0_admission_refused` — Admission refused | A kind with no registered executor is refused before any task, attempt, event or publication row lands. | `m0_fault_admission.rs` |
| `m0_admission_duplicate` — Admission duplicate | The identical request admits one task; a conflicting body under the same id is refused without rewriting the stored row. | `m0_fault_admission.rs` |
| `m0_replay_no_admission` — Replay, no admission | Reopening a completed run's database with a fresh host, recovering and running produces no attempt, event, publication or effect. | `m0_fault_admission.rs` |
| `m0_approval_stale_dispatch` — Stale approval dispatch | A pending, expired, cancelled or already-consumed authorization, and an answered one whose generation the execution does not name, are refused at the receipt gate; the answered one spends exactly once. | `m0_fault_approval.rs` |
| `m0_approval_persisted_replay` — Approval persisted, replayed | An approval answered before any dispatch survives the restart; recovery interrupts queued work instead of running it, and an explicit resume dispatches once. | `m0_fault_approval.rs` |
| `m0_receipt_uncertain` — Receipt uncertain | The attempt is dropped after the effect was admitted and before its result was recorded; the reopened host records it uncertain and refuses the identical effect. | `m0_fault_effect_receipt.rs` |
| `m0_receipt_committed` — Receipt committed | The result is recorded before the attempt ends; completing it again is idempotent and reopening serves the record instead of executing it. | `m0_fault_effect_receipt.rs` |
| `m0_publish_gap` — Publication gap | A gapped sequence is accepted, a regressing sequence and a repeated event id are refused, and neither refused write commits a prefix. | `m0_fault_publication.rs` |
| `m0_ack_delivery` — Acknowledgment delivery | A late acknowledgment under an expired claim is refused; the record is delivered once by the holder of the live claim. | `m0_fault_publication.rs` |
| `m0_old_cursor_reconnect` — Old-cursor reconnect | Reconnecting with the pre-restart cursor serves the durable suffix and claims nothing. | `m0_fault_publication.rs` |
| `m0_cancel_queued` — Cancel queued | A task cancelled before any claim settles cancelled with no attempt, effect, event or publication. | `m0_fault_cancellation.rs` |
| `m0_cancel_uncertain_effect` — Cancel uncertain effect | Cancellation keeps committed evidence, leaves the admitted effect uncertain, records no rollback, and the restarted host replays nothing. | `m0_fault_cancellation.rs` |
| `m0_record_corrupted` — Corrupted record | A receipt whose stored bytes no longer describe a receipt fails both the read and a fresh admission for the identical effect. | `m0_fault_corrupt_record.rs` |

Each boundary test asserts durable state — stored task, attempt, event,
publication, agent-input and receipt rows read back through the public store
API, or the client-visible projection after a reopen — and never a helper's
return value. Four of them name an M0 gate property directly:
`history_replay_never_admits_work`,
`a_stale_approval_can_never_authorize_a_dispatch`,
`cancellation_settles_truthfully_and_never_assumes_rollback`, and
`an_uncertain_effect_is_never_replayed`.

The six differential profiles above remain the reviewed TypeScript/native
comparison set, with the same drift check and the same six-entry native report.
The `m0_*` profiles are additive: they are graded against their own reviewed
checkpoints, not against the TypeScript runner.

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
The `m0_*` profiles close part of that last sentence — the crash between an
external write and its recorded result is now asserted by
`m0_receipt_uncertain`, and approval, acknowledgment, cursor and corrupt-record
boundaries are asserted at all — but they inherit its other limits: recovery is
manual, effects remain deterministic executor callbacks rather than live
provider, MCP or domain-service requests, and the drivers are one in-process
host plus a reopened store, not two cooperating processes.

Still outside this schedule, and unexecutable without a source-level hook
(see the M0 report): a store that reports full or unavailable
(`SQLITE_FULL` has no public knob), subprocess-descendant termination and PID
reuse, a deadline that expires while a real subprocess continues, and a slow
MCP `call_tool`.

## Authoring copy

The six differential profiles are duplicated at `rust/fixtures/conformance/`,
which is the copy the TypeScript drift test rebuilds and requires to equal its
model with exactly six entries. The `m0_*` profiles are authored in this crate's
copy and read by the native `m0_fault_*` tests through `include_str!`; adding
them to the drift-checked copy requires authoring the matching checkpoints in
`scripts/tests/rust-runner-conformance-model.ts` first.

## Run

```sh
bun test scripts/tests/rust-runner-conformance.test.ts
cargo test --manifest-path rust/Cargo.toml -p agentkit-host --test runner_conformance --locked --offline
cargo test --manifest-path rust/Cargo.toml -p agentkit-host --test 'm0_fault_*' --locked --offline
AGENTKIT_RUN_RUST_RUNNER_CONFORMANCE=1 bun test scripts/tests/rust-runner-conformance.test.ts
```

The enabled differential gate runs the native test binary, including its killed
child process, writes an actual native JSON report to a temporary path, and
compares it against fresh actual TS execution. Common profiles must match each
other and their reviewed expectations exactly. Approved differences must match
their separate expectations exactly and must remain observably different.
