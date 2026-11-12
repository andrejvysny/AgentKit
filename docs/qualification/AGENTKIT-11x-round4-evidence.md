# AGENTKIT-107 round 4 — assembled-path hardening evidence

**AgentKit revision:** `df5130b` (`feat/claude-code-backend`;
OneMind re-vendored at this exact commit, `vendor/agentkit-rust.provenance.json`:
677 files, `dirty: false`, byte-exact provenance regenerated from the
clean checkout — no post-generation formatting).

**Scope.** Fourth external review of the Claude Code native backend.
It confirmed the round-3 closures (protocol tolerance, per-attempt
cleanup, EOF ownership, durable owner rules, replay identity, admission
validation) and reproduced nine NEW gaps in the ASSEMBLED path: F1
advertised endpoint, F2 installed approval deadline, F3 continuation
claims, F5 normal-completion descendants, F7 broker replay/cancellation/
credit, F8 admission-path consistency, F9 two public protocol shapes,
F13 profile-qualification load path (plus the OneMind-side F4/F6/F10/
F11/F12 evidenced in the OneMind round-4 doc). Every finding was
re-reproduced here as a failing-before test, then fixed; none is closed
by constants or comments alone.

## F1 — The advertised MCP endpoint actually serves (`host/native_bridge.rs`, `mcp/src/server.rs`)

`AttemptBroker::start` advertised `http://127.0.0.1:<port>/mcp` while
`McpServer::router()` mounted only `/` — the exact returned URL answered
**HTTP 404** (reviewer loopback repro). `McpServer` now exposes
`router_at(path)`; the broker serves
`server.router().merge(server.router_at("/mcp"))` — advertised route =
mounted route, `/` kept for compatibility. The endpoint test is no
longer "URL string + token string":
`advertised_endpoint_serves_initialize_list_and_call` performs the REAL
streamable-HTTP handshake (JSON-RPC `initialize` → `notifications/
initialized` → `tools/list` → `tools/call`) against the EXACT
`spec().http_url` with the generated bearer token, asserting the
scripted authority executed. Direct `call_tool()` unit tests remain,
but they are no longer the evidence for the HTTP integration.

## F2 — The approval deadline is the INSTALLED configuration (`host/native_bridge.rs`, `mcp/src/server.rs`)

Round 3's R5 claimed the ladder made "a 300-second approval time out
nothing"; the constant chain was compatible, but the server was still
constructed with `max_call_duration = call_timeout_ms` — the 120 s
EFFECT timeout enclosing authorize+execute (reviewer repro: 50 ms
effect budget, 500 ms authorize budget, 120 ms decision → failed at
53 ms). The installed deadline is now DERIVED:
`broker_call_deadline(limits) = authorize budget + effect timeout +
BROKER_SERVER_DEADLINE_OVERHEAD_MS (30 s)`, recorded and exposed via
`server_call_deadline()`, while the inner authorize/execute gates stay
individually bounded. `approval_longer_than_effect_timeout_serves_
through_the_server` assembles a real broker (`call_timeout_ms = 50`,
120 ms scripted decision) and completes `tools/call` over the
advertised URL, asserting the installed deadline covers the whole
chain — the old wiring dies at the effect timeout.
`deadline_chain_stays_compatible_by_construction` is retained and
extended (overhead + installed default) but its doc now points at the
assembly test instead of claiming to be the proof.

## F3 — Continuations are CLAIMED, not observed (`host/native_executor.rs`, `store/native_continuations.rs`, `store/schema.rs`)

The review's three reproductions all came from reading an input binding
without consuming it. The ledger now implements the prescribed state
model at schema **version 19** (`owned`/`uncertain` states +
`claimed_attempt_id`/`claimed_task_id`/`claimed_at`/`generation`
columns, additive upgrade-preserving migration —
`v18_ledger_upgrades_to_the_claim_model_without_losing_rows`):

- **Atomic claim.** `claim_continuation` is a lease-fenced
  compare-and-set `active → owned` keyed by the full scope tuple +
  claimant, executed in the transaction under the CURRENT lease: two
  attempts on one position produce exactly one winner
  (`two_current_tasks_cannot_consume_one_binding`); the loser takes the
  fresh-session path.
- **Interrupted after input ⇒ uncertain.** The executor claims BEFORE
  spawn and discharges on EVERY terminal arm: interrupted/uncertain
  after input → `mark_continuation_uncertain` — exact resume from the
  pre-claim position is REFUSED even though the same-session alias
  still exists (`interrupted_after_input_cannot_exact_resume_the_pre_claim_position`);
  input never accepted (spawn/encode failure) → `release_continuation_claim`
  back to available. The same-session alias sweep at commit also covers
  interrupted/retired attempts.
- **The observed binding proves identity only.** The task-local
  `current_native_binding` shortcut no longer bypasses the ledger: the
  executor demands `resumable_continuation` (active, or owned by THIS
  attempt, full config-identity + prompt-revision match). A retired,
  superseded, uncertain or foreign-owned row is a miss — the same-task
  Resume bypass (OneMind's `host.resume(run_id)` route) now reaches
  `continuation_mismatch` with zero OS contact
  (`retired_continuations_refuse_the_same_task_resume`).

## F5 — Normal completion reconciles the process group (`claude/src/transport.rs`, `native/src/ports.rs`, `host/native_executor.rs`)

The repaired abandonment paths kept their behavior; the NORMAL `wait()`
path now reconciles the owned group BEFORE finalize/deregistration:
`group_live_scan` distinguishes `Some(0)` (VERIFIED empty) from `None`
(scan untrusted — never certainty), and `reconcile_group_after_leader_
exit` runs SIGTERM → bounded grace → SIGKILL → verify-empty on the
leader's group, preserving the leader's exit facts untouched. Uncertainty
is reported, not swallowed: the supervisor records an `InterruptReceipt`
per (pid, generation) and the port gained a defaulted
`ProcessSupervisor::descendant_cleanup(pid, generation)` read-back
(`None` = no report ≠ clean); the executor ORs it into the attempt
record's `cleanupUncertain` — `unconfirmed_descendant_cleanup_marks_
completed_attempt_uncertain` (completed turn, only source is the
supervisor report) and `reported_nothing_keeps_completed_attempt_certain`
(default trait method pins pre-F5 behavior). OS regression:
`normal_completion_reaps_same_group_descendants` — leader with a
same-pgid heartbeat descendant exits 0 after a valid root result; after
`wait()` + drops the descendant is gone and the heartbeat file size is
FROZEN (reviewer measured 4→9 bytes growing before the fix; the test
fails exactly that way with the reconcile removed).

## F7 — Replay, cancellation and credit are honest (`host/native_bridge.rs`)

- **Reservation cleanup is cancellation-safe.** A RAII
  `ReservationGuard` + `effect_started` flag: abandonment BEFORE the
  effect releases the inflight reservation (redelivery is a legitimate
  fresh call — `cancelled_authorization_releases_the_replay_reservation`);
  abandonment where the effect MAY have started keeps a durable
  UNCERTAIN memo whose redelivery returns in-band `effect_uncertain` —
  never `duplicate_in_progress` forever, never re-execution
  (`abandoned_effect_reports_uncertainty_and_never_reexecutes`).
- **Approval credit is the UNION of waits.** Two overlapping parks no
  longer double-credit (reviewer measured 122 ms credit in 61 ms wall):
  an `active`/`base` pair opens the union on 0→1 and flushes on 1→0
  (`overlapping_parks_are_credited_as_a_union`).
- **Replay verifies CONTENT.** Memos store the tool name and the
  canonical digest of the arguments; a proven-redelivery key restated
  with different tool/args fails closed with in-band `replay_conflict`
  instead of serving the first request's result
  (`replay_never_serves_a_restated_request_with_different_content`);
  faithful restatements still replay byte-for-byte exactly once.

## F8 — One admission contract for both paths (`host/src/session/mod.rs`, `host/native_executor.rs`)

Immediate and queued submissions now share ONE `admission_envelope` /
`admission_digest` (input id + frozen stanza + content): repeating an
input id with a swapped frozen target on the IMMEDIATE path yields the
same `input_conflict` the queued path already produced, instead of a
silent replay of the old turn (`native_admission_round4.rs`). The
executor's cross-field checks became `validate_frozen_semantics` — the
single semantic validator now run at ADMISSION before any row persists
and again at executor step 3b: a typed continuation scope contradicting
the execution target's backend/executable/config-identity is refused
durably-clean (no task, no input row — the reviewer's
"accepted then refused later" class cannot recur).

## F9 — Two public message shapes are normalized (`claude/src/protocol.rs`)

`redacted_thinking` blocks inside assistant (BetaMessage) content are
retained verbatim as OPAQUE values with zero field requirements — never
reconstructed, never displayed, never blocking siblings: a fixture with
[redacted_thinking, text] surfaces the visible text
(`redacted_thinking_block_is_opaque_and_never_blocks_visible_text`;
bounded marker retention 32 + dropped counter; child-scoped withheld
blocks excluded). The documented `model_refusal_no_fallback` system
notification dispatches as a non-fatal `NativeEvent::Warning` — the
turn stays `Uncertain` until the authoritative `result` decides
(`model_refusal_no_fallback_is_informational_until_the_result`).
Authority-affecting messages (init, result, tool governance) and
undocumented subtypes (`model_refusal_fallback`) stay strict.

## F13 — Profile qualification is queryable AND loadable (`claude/src/discovery.rs`)

The ledger was already honest; it was unusable by consumers:
`recorded_qualification_for` now resolves in two documented tiers
(newest EXACT-profile record first, then newest `"all"` — an "all"
record can no longer mask the exact record's evidence). Evidence enters
the process-local ledger through a SUPPORTED durable path: the
schema-stable `QualificationRecord` (wire fields `version`, `os`,
`profile`, `qualifiedAt`, `source`; extras ignored) plus
`load_qualification_records` / `load_qualification_json` /
`load_qualification_file` — merge-never-clear, newest-`qualifiedAt`
wins, strict RFC 3339 stamps (mandatory zone, real calendar bounds),
blank fields refused. Eight tests (versions 2.1.910–2.1.924) pin
per-profile isolation, exact-preference exposure, `"all"` fall-through,
idempotent JSON loads, file refusals, and malformed-record rejection.
The production ledger remains INTENTIONALLY EMPTY: unqualified until a
real qualification run is recorded — this doc records no live
qualification.

## Verification

```
cargo fmt --all                                  clean
cargo clippy --workspace --features agentkit-host/http \
  --exclude agentkit-napi --all-targets -- -D warnings   clean
cargo test --workspace --features agentkit-host/http --exclude agentkit-napi
  → 1886 passed / 0 failed (full log capture, zero failure lines)
```

One transient failure appeared in an earlier full run while a sibling
cargo build hammered the machine (fail-fast truncated the run before
the log captured the name); three subsequent FULL runs — including this
gate with complete log capture — show zero failures. The historical
`/proc`-assumption fixtures remain the known environment-limited
population from round 3.

| Suite | Result |
| --- | --- |
| workspace total | **1886 passed / 0 failed** |
| `agentkit_host --features http` (lib) | **149 passed / 0 failed** (round-4 additions incl. endpoint, assembly-deadline, claims, policy carrier, admission digest) |
| `agentkit_store` (full incl. `continuation_claims.rs`) | **327 passed / 0 failed** |
| `agentkit_claude` (protocol/transport incl. F5/F9) | **102 passed / 0 failed** |
| `agentkit_mcp` (router incl. `/` suites) | **124 passed / 0 failed** |

Limits (honest): loopback MCP only — no authenticated Claude CLI was
invoked; `/proc`-assumption fixtures keep their environment-limited
status from round 3 and the F5 case runs under live OS liveness
polling; F9 fixtures are public-contract shapes, not captured vendor
traffic; no macOS validation in this container.
