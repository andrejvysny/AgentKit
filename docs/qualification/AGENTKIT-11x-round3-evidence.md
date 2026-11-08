# AGENTKIT-110/111/115/112/113/114/118 — Round 3 hardening evidence

**AgentKit revision:** `8fc2ef5e` (`feat/claude-code-backend`;
OneMind vendored at this exact commit,
`vendor/agentkit-rust.provenance.json`: 675 files, `dirty: false`;
only docs-only commits sit on top).

**Scope.** This closes the independently reproducible findings of the
re-review conducted on the earlier revision `97acdcbb` — R1 (protocol),
R2 (process ownership), R4 (durable authority), R5 (broker deadlines),
R7 (replay identity), R9 (session contract), R10 (projection, error
detail, history budget). R3/R6/R8 were OneMind-side findings; their
AgentKit counterparts are covered here and in the OneMind round-3
evidence doc. Plane tracks used: AGENTKIT-110/111 (R1, R2),
AGENTKIT-115 (R4), AGENTKIT-112/113 (R5, R7), AGENTKIT-114 (R9, R10),
AGENTKIT-118 (qualification ledger).

## R1 — Claude protocol tolerance (`crates/claude/src/protocol.rs`)

- `on_rate_limit_event` accepts the published wire shape: object
  `rate_limit_info` with a **numeric** `resetsAt` (the legacy
  `rate_limit` envelope name is kept as an alias; string-encoded digits
  are preserved); the earlier `rate_limit`-only / string-`resetsAt`
  expectation was wrong. Malformed progress values become a non-fatal
  `NativeEvent::Warning`, never a terminated turn. Dedicated protocol
  tests:
  `rate_limit_and_status_and_compact_boundary_are_tolerated_progress`,
  `tool_progress_heartbeats_are_nonterminal_metadata`,
  `progress_values_malformed_warn_but_never_terminate`
  (`crates/claude/src/protocol.rs`).
- `tool_progress` frames dispatch as **nonterminal metadata** (dedicated
  dispatch arm; also listed in the tolerated-progress set so a malformed
  progress value warns without terminating): a 30-second foreground
  heartbeat can no longer terminate a healthy tool turn. Completion-,
  ownership- and authority-affecting messages remain strict.

## R2 — Attempt-scoped cleanup and EOF ownership

- `AttemptOwner::drop` terminates **only its own generation**
  (`supervisor.terminate_owned(generation, policy)`); it never drains
  the shared supervisor. Reviewer reproduction (drop A interrupts B)
  now passes: B's process is never signalled (`terminated.get() ==
  false`), and an unknown/stale generation is refused with `stale_attempt`
  (`crates/claude/src/transport.rs`,
  `crates/host/src/native_executor.rs`).
- Whole-runtime shutdown still drains everything: the supervisor `Drop`
  arms every retained orphan (including a deliberately **leaked**
  `OwnedChildHandle`), then group-kills its scope **synchronously**
  (`kill(-pgid, SIGKILL)`); the reaper task reaps. New regression test
  `supervisor_drop_sweeps_orphaned_children` polls
  `/proc/<pid>`/`WaitStatus::StillAlive` and fails if any vendor child
  outlives shutdown.
- **EOF is not exit.** `next_frame` records `stdout_eof`; drop cleanup
  proceeds when the handle is unreaped OR the last frame was **not** a
  confirmed `Result` — EOF without a result keeps kill/wait/take ownership
  (`cleanup_uncertain` recorded when kill succeeded but exit could not be
  re-confirmed). Only a confirmed terminal `Result` disarms drop.

## R4 — Durable owner rules (`crates/store/src/*`)

`native_exec::live_authority_gap` is now the single current-owner rule
used consistently — correct task and attempt, current lease generation,
**unexpired lease**, **no cancellation request**, appropriate executing
task state, checked inside the transaction. The continuation commit
(`crates/store/src/native_continuations.rs`) and the fenced interaction
answer (`crates/store/src/native_exec.rs`) both consult it.

- Reproduced reviewer case — commit after cancel + lease removal — now
  refuses (`commit_requires_current_owner_authority` in
  `crates/store/src/native_continuations.rs`): no active, resumable
  binding is created, and expired leases alone are also a refusal.
- Fenced interaction answer after cancel-request/lease-expiry is refused
  (`fenced_answer_refuses_cancel_requested_and_expired_lease` in
  `crates/store/src/native_exec.rs`).

## R5 — Compatible deadline ladder (`host/native_bridge.rs`, `claude/launch.rs`)

`BROKER_AUTHORIZE_BUDGET_MS = 360 s` (≥ 300 s human approval window +
30 s clamp margin), broker `call_timeout_ms = 120 s`, and
`BROKER_MCP_REQUEST_BUDGET_MS = 540 s` (> authorize budget + call
timeout). A 300-second approval can no longer time out the outer MCP
call before the advertised window elapses;
`deadline_chain_stays_compatible_by_construction` locks the ladder
(compile-time `const { assert! … }` for the approval-window leg).
Cancellation/timeout/write-failure on the authorize and execute paths
pair every `live_call` with `finish_live_call` — no abandoned park leaks
gauge or credit (`abandoned_park_leaks_no_gauge_or_credit`,
`unanswered_authorize_hits_the_gate_deadline`; `live_calls()` asserted
zero on every abnormal path in the bridge suite).

## R7 — Typed replay identity (`host/native_bridge.rs`)

The replay memo keys are `MemoKey { actor_id, request_id: RpcId }` —
JSON-RPC ids are stored **typed** (`RpcId::from_request_id`,
`crates/mcp/src/handler.rs`), never stringified, so numeric `1` and
string `"1"` are distinct keys (JSON-RPC ids are session-scoped and may
be strings or numbers). The earlier stringified collision (session B
reusing session A's cached `1`) cannot recur:
`memo_never_collapses_numeric_and_string_request_ids`,
`memo_is_scoped_to_the_session_that_earned_it`,
`same_session_same_typed_id_replays_exactly_once_executed`
(`crates/host/src/native_bridge.rs`).

## R9 — Session contract validation at admission (`host/src/session/mod.rs`)

`validate_frozen_stanza` runs at **creation**, at **send**, and over
**inherited** stanzas. A malformed queued `executionTarget` (e.g.
`"garbage"` instead of the tagged shape) is refused with
`invalid_input` at both layers — creation refuses to persist it, and the
send path refuses a descriptor half-seeded by a legacy upgrade
(`malformed_session_native_stanza_is_refused_at_creation` in
`crates/host/tests/native_session_lane.rs`).

## R10 — Vendor error detail (`crates/claude/src/protocol.rs`)

**Vendor error details preserved**: an error `result` frame's
`errors[]` array is collected by `result_errors` (bounded retention —
first entries up to a cap) and appended to the projected failure message
as `— vendor errors: {detail}`; `is_error` drives the terminal outcome.
An error result with details but no `result` text no longer collapses to
a generic subtype body
(`error_result_details_are_preserved_and_classified`).

The remaining R10 items from the re-review are OneMind-side projection
and context-assembly fixes (authoritative-attempt-first reduction in
`native_chat/executor.rs`, seed-history budget marker, single-emission
template/skill aggregate, artifact pointer disclosure, `max_vendor_turns`
default) and are evidenced in the OneMind round-3 evidence doc.

## Qualification honesty (`crates/claude/src/discovery.rs`, AGENTKIT-118)

`is_qualified(version)` / `is_qualified_for(version, profile)` answer
**only** from recorded qualification evidence
(`record_qualification(QualificationEvidence { cli_version, … })` — the
recorded output of an actual qualification run against the official
CLI). A version merely *observed* — `--version` output, support-matrix
membership, or the minimum-version floor — is never treated as
qualified, and evidence never transfers across versions, operating
systems, or profiles (`is_qualified_never_answers_from_the_version_ledger`,
`in_ledger_version_without_recorded_qualification_is_not_qualified`,
`recorded_qualification_quals_that_exact_version`,
`recorded_qualification_does_not_transfer_between_versions`,
`qualification_does_not_transfer_between_operating_systems`,
`qualification_does_not_transfer_between_profiles`). OneMind's
ClaudeNativePanel surfaces this distinction to the user.

## Verification

```
cargo fmt --all                                  clean
cargo clippy --workspace --features agentkit-host/http \
  --exclude agentkit-napi --all-targets -- -D warnings   clean
cargo test --workspace --features agentkit-host/http --exclude agentkit-napi
```

Gate (current container; `/proc`-assumption process fixtures run under
live OS liveness checks):

| Suite | Result |
| --- | --- |
| workspace total | **1854 passed / 0 failed** |
| `agentkit_claude` (lib incl. protocol + transport) | **91 passed** |
| `agentkit_host` (lib) | **136 passed** |
| `agentkit_store` (lib) | **69 passed** |
| `native_session_lane` (R4/R9) | **9 passed** |
| `chat_continuation_matrix` (R3) | **10 passed** |
| `cleanup_uncertainty` / `quarantine_race` / `managed_cleanup` | **3 / 2 / 4 passed** |
| `protocol` (R1) / `discovery` | **9 / 1 passed** |

Three `/proc`-assumption process fixtures remain environment-limited as
in the re-review and are covered by the OS-liveness replacements above.
