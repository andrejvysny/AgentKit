# AGENTKIT-118 — Standalone Claude Code backend qualification evidence

Status: **LOCAL SUITES RECORDED; LIVE QUALIFICATION BLOCKED (no `claude` executable on this host); FINAL DIGEST PENDING.**

Authority: this file records executed evidence only. Anything not executed here is
listed under *Blocked/Not executed* with the exact missing prerequisite. Plane
AGENTKIT-107/118 remain the task/decision authority; the scoped Page
`Claude Code - native backend for general Chat` (project 9b59c292) is the design
authority. APIs, protocols and test procedures live in Git:
`rust/crates/{native,claude,host,mcp}` + this directory.

## 1. Source revision (frozen candidate)

| Field | Value |
| --- | --- |
| Repo | `AgentKit` (local working copy `/home/students/xvysnya/workspace/AgentKit`) |
| Branch | `feat/agent-workspace-loop` |
| Frozen commit | `8b6741ed43187c71642c563802290579298a8ecd` |
| Uncommitted delta | new crates `native`, `claude`; additions to `host`, `store`, `mcp` (see §7 file list). Final source digest to be recorded at delivery close (no commits/pushes per user Git policy). |
| Rust toolchain | cargo/rustc 1.97.0 |
| Build target dir | `/tmp/ak-target` (Cargo target dir for all runs below) |
| Vendor/lockfile | `Cargo.lock` unchanged by new crates (workspace path deps only; no new external deps) |

## 2. Environment

Linux x64 (host `linux 7.0.0-38-generic`). **No official Claude Code CLI
installed** (`which claude` → empty; npm global list has no `@anthropic-ai/claude-code`).
All suites below are hermetic: they execute against scripted in-process ports
(`ScriptedBackend`/`FakeSupervisor`/`FakeLocator`) or OS-level fakes (shell
children, loopback sockets), never against the real CLI.

## 3. Executed local suites (green at time of recording)

**Workspace gates (final, review round 2026-11-07):**
`cargo test --workspace --all-targets --exclude agentkit-napi --features agentkit-host/http --no-fail-fast`
→ **every target green (exit 0)**; `cargo clippy --workspace --all-targets --exclude agentkit-napi
--features agentkit-host/http -- -D warnings` clean; `cargo fmt --all -- --check` clean.
The single exclusion is `agentkit-napi` (Linux standalone link: `undefined symbol:
napi_release_threadsafe_function` — Node NAPI symbols are unavailable outside a Node
addon host; pre-existing, documented, never vendored to OneMind).
Two previously-recorded "environment" failures were root-caused and **fixed this round**,
not excluded:
- `schema::tests::orphaned_legacy_owner_locks` — `lock_file` wrote the owner tag only
  into freshly created (empty) lock files, so a recycled inode inherited a dead
  database's tag and the sweep misclassified the LIVE lock. Repair now happens under
  the acquired flock (`crates/store/src/schema.rs`), and the test creates all four
  fixture databases before freeing any inode (the filesystem recycles just-freed
  inodes immediately). 6× green isolated + in-suite runs.
- `durability::acknowledged_batch_survives_killed_writer{,_with_fsync_noop}` —
  `read_bypassing_cache` used the macOS `F_NOCACHE` fcntl (48), which is UNASSIGNED
  on Linux → `EINVAL` on every Linux run. The cache-bypass leg is now
  `#[cfg(target_os = "macos")]`; Linux asserts the WAL image as the kernel presents
  it (the fsync-firing claim remains falsified by the `faults` pragma unit tests, as
  the test's own platform note already documented).

| Suite | Crate | Tests | Covers (AGENTKIT task) |
| --- | --- | --- | --- |
| contracts | `agentkit-native` | 27 | 108: target schema/versioning, capability validation before spawn, event/outcome vocabulary, digests, model-loop untouched |
| discovery/auth/profiles | `agentkit-claude` | 21 | 109: locator precedence + digest, six auth methods parsed, managed-policy detection, support-matrix floor 2.1.259, recheck_replaced, isolated profile argv |
| transport | `agentkit-claude` | 11 | 110: group-aware shutdown ladder, generation fencing, immediate stdout/stderr readers, bounded frames/stderr/stdin, drop-kills-descendants, blocked-pipe shutdown |
| stream adapter | `agentkit-claude` | 19 | 111: NDJSON normalization, delta/complete reconciliation, unknown-tolerance, denial/usage/retry capture, EOF-no-result never Completed |
| driver integration | `agentkit-claude` | (incl. above; 69 crate total) | launch plan composition per profile |
| host library | `agentkit-host` | 121 lib (`--features http`) | 112/113/115/116: admission parse+verdict, broker (24), executor lifecycle (21 incl. §8 regressions): completion, EOF-no-result, cancel, interrupt, startup/idle/overall timeouts, quarantine, broker wiring, dispatch routing (incl. malformed-target terminal). Default-feature build: 86 lib tests (native modules are `http`-gated) + 21 conformance lane. |
| store ledger | `agentkit-store` | 5 tokio tests (`native_exec.rs`) | 112/115: binding freezes identity + confirms once, binding requires task+identity, OBSERVED-ONLY cross-task resume authority (`is_observed_only_after_vendor_echo`), answer is CAS with full identity binding, expiry and invalidation are durable |
| facade + driver conformance | `agentkit-facade` | 9 (2 conformance + 7 facade) | 117: `NativeChat` standalone preflight/chat_turn PlainChat-only; 10-case driver conformance suite run against the real claude driver |
| standalone lifecycle example | `agentkit-host` example | run green | 118: durable admission → claim → spawn → events (`native.session.bound`, `native.spawn`, `native.event`×3, `native.completed`) → `Completed` + lineage `observed` |

## 4. Stands-alone backend proof (Page §8.1)

* Plain-Chat profile: `NativeAgentExecutor` with `NativeChatConfig::plain_chat()`
  executes a complete turn over scripted ports → `Completed` with reconciled
  assistant text, usage provenance, `session_id` bound pre-spawn and promoted on
  `init` (`bind_session` → `observe_session` ledger transitions asserted).
* No native coding-tool execution is possible: launch plan pins
  `--tools ''`, `--strict-mcp-config`, `--permission-mode default`,
  `--permission-prompt-tool` set whenever a broker is attached, and
  capability policy rejects `native-coding-tools`/`live-permission-prompt`/
  `deferred-tools` requests.
* `examples/native-chat-lifecycle.rs` runs the whole admission→dispatch→
  completion flow against fakes (`cargo run -p agentkit-host --example
  native-chat-lifecycle`).

## 5. Blocked live gates (not substituted)

| Gate | Blocked by | Exact check that must run when unblocked |
| --- | --- | --- |
| Live discovery/auth | no `claude` executable on Linux host | `ClaudeLocator::default().locate()` digest; `discover_authentication` against real `claude auth status` output; managed-settings detection on a managed machine |
| Live stream qualification | no CLI + no authenticated account | record NDJSON of a real plain-Chat turn; adapter normalization diff vs fixtures; delta/complete reconciliation on real usage |
| Live tool bridge | no CLI | real `--mcp-config` loopback dispatch, approval round-trip, revocation |
| Live crash/resume | no CLI | kill -9 mid-turn → requeue; exact resume lineage vs fresh; stop/interrupt ownership |
| macOS packaging | Linux host | ONEMIND-346 owns packaged qualification |
| Credential expiry UX | no CLI | sign-out/expiry transitions from real `auth status` |

Mocked suites are never counted as live evidence; these rows stay open until run
against the official executable with a user-authenticated account.

## 6. Command log (reproduction)

```
cd /home/students/xvysnya/workspace/AgentKit/rust
CARGO_TARGET_DIR=/tmp/ak-target cargo fmt --all -- --check
CARGO_TARGET_DIR=/tmp/ak-target cargo clippy --workspace --all-targets -- -D warnings
CARGO_TARGET_DIR=/tmp/ak-target cargo clippy -p agentkit-host --features http --all-targets -- -D warnings
CARGO_TARGET_DIR=/tmp/ak-target cargo test --workspace --exclude agentkit-napi   # 1100 passed / 1 pre-existing env failure
CARGO_TARGET_DIR=/tmp/ak-target cargo test -p agentkit-native        # 27 passed
CARGO_TARGET_DIR=/tmp/ak-target cargo test -p agentkit-claude        # 69 passed
CARGO_TARGET_DIR=/tmp/ak-target cargo test -p agentkit-host --features http      # 117 lib + lanes
CARGO_TARGET_DIR=/tmp/ak-target cargo test -p agentkit-store --lib native_exec   # ledger CAS
CARGO_TARGET_DIR=/tmp/ak-target cargo test -p agentkit-facade        # 9 passed
CARGO_TARGET_DIR=/tmp/ak-target cargo run -p agentkit-host --features http --example native-chat-lifecycle
```

## 7. File inventory (this delivery, AgentKit side)

New: `rust/crates/native/src/{canonical,capability,error,events,interaction,lib,ports,target}.rs`,
`rust/crates/claude/src/{auth,backend,discovery,error,launch,lib,profile,protocol,support,transport}.rs`,
`rust/crates/host/src/{native_bridge,native_executor}.rs`,
`rust/crates/store/src/native_exec.rs`,
`rust/crates/host/examples/native-chat-lifecycle.rs`,
`rust/crates/facade/**` (AGENTKIT-117), `docs/adr/0021-*.md`, this file.
Modified: `rust/Cargo.toml`, `crates/host/{Cargo.toml,src/lib.rs,src/ports.rs,src/bin/server.rs}`,
`crates/store/{Cargo.toml,src/lib.rs}`.

Follow-up (consumed by the OneMind lane, ONEMIND-340): `ExecutionContext::lease_fence()`
(`crates/host/src/lib.rs`) exposes the attempt's live `LeaseFence` so lane-side
executors can write projected rows through the store's fenced task APIs
(`append_projected`) under the runner-published lease. Re-verified:
`cargo test -p agentkit-host` (all suites green), `cargo fmt -p agentkit-host --check`,
`cargo clippy -p agentkit-host --all-targets` clean; the vendored copy in
`OneMind/vendor/agentkit-rust` is byte-identical to this tree.

## 8. Independent-review round (2026-11-07)

Findings from the review of the integrated delivery, each fixed with a regression
test that fails on the pre-fix code:

| Finding | Fix | Regression test |
| --- | --- | --- |
| P1-1 preflight failed closed on informational readiness notes | `native_executor.rs` step 5 refuses ONLY on `blocking_issues` (terminal, non-retryable: operator fix); notes never block | `informational_preflight_notes_never_block_the_turn`, `blocking_preflight_issue_refuses_without_spawning` |
| P1-2/F1 cross-task `Exact` resume refused with `lineage_missing` (binding lives on the PREVIOUS task) | executor consults `SqliteStore::is_observed_native_session` (new, `native_exec.rs`): resume authority = vendor-echoed (`observed`) anywhere, never `assigned`-only | `cross_task_exact_resume_uses_previous_tasks_lineage` (host), `is_observed_only_after_vendor_echo` (store) |
| P2-3/F3 fresh sessions became `observed` at spawn — a pre-`init` death minted resumable phantom sessions | fresh turns bind `assigned`; only the vendor `init` frame (or an `Exact` restatement) promotes to `observed` | `unobserved_assigned_session_is_never_resumable` (also asserts the phantom `Exact` resume is refused with zero spawns) |
| F5/P2 a frozen-but-malformed `executionTarget` silently fell through to the model-loop fallback (wrong execution owner, wrong billing) | `NativeDispatchExecutor` refuses terminal `execution_target_invalid` whenever the key is present but unparseable; only a legacy target-less task reaches the fallback | malformed-native case in `dispatch_executor_routes_by_frozen_target` (both executors' hit counts asserted) |
| P2-1 vendor child inherited ambient `ANTHROPIC_*`/`CLAUDE_CODE_*` credential families; auth probes trusted poisoned env | transport strips both families BEFORE pinned additions; `auth_status` probes run with the same scrubbed env (`credential_env_keys`) | `launch.rs` env assertions (`env_additions_are_noncredential_and_scrub_target_recorded`) + transport scrub tests |
| P3-2 per-run 0700 dir leaked on post-plan failure paths | `settle_cleanup` takes `Option<&Path>` and removes the plan's directory on EVERY terminal path where the plan exists; plan-build failure cleans its own dir | all six settle call sites pinned by the executor lifecycle suite (temp-dir root per test) |
| P3-1 capability policy didn't refuse the contract's out-of-scope names | forbidden list extended to `live-permission-prompt` + `deferred-tools` (refused even if a future CLI advertised them) | forbidden-name path in `unknown_capability_is_transient_structural_is_terminal` matrix |
| Store finalize reads needed a tail (OneMind ONEMIND-342 consumer) | `SqliteStore::events_tail` (ascending last-N, same ceiling as `events_after_number`, `limit 0` = empty) | `events_tail_returns_the_last_events_ascending` (ordering, over-request, zero, missing-task error) |
| Owner-lock tag inheritance on recycled inodes (§3) | tag repair under flock + deterministic fixture | `orphaned_legacy_owner_locks…` 6× green |
| `F_NOCACHE` macOSism broke Linux durability legs (§3) | platform-split cache-bypass reader | both killed-writer tests green on Linux |
| v17 native ledger broke every raw-SQL downgrade fixture | fixture drop lists extended in `store/tests/{agents,migration,subagents,task_indexes,teammates}.rs`, `toolpacks-coding/tests/legacy_upgrade.rs`; `SCHEMA_VERSION` tripwire 16→17 | all legacy-upgrade targets green |

Post-round gate (exit 0): the §3 no-fail-fast workspace run, clippy `-D warnings`
clean, fmt clean. Vendored to OneMind byte-identically
(`vendor/agentkit-rust`, 672 files, `--check` OK).

## Review round 2 (2026-11-08, commit d158ee1) — independently executed

Findings of the second review (protocol events, literal input, seed history,
text assembly, frame bounds, ownership, scoped continuations, dispatch
capabilities, discovery tiers, system-managed policy, deadline/parked-approval
model), each fixed with a regression test that fails on the pre-fix code.

| Gate | Result |
| --- | --- |
| `cargo test --workspace --features agentkit-host/http --exclude agentkit-napi` | green (exit 0) |
| `agentkit-claude` | 76 passed |
| `agentkit-host` (incl. `native_session_lane`, broker lifetime, parked-approval credit) | green |
| `agentkit-store` (schema v18 `native_continuations`, migration fixtures extended) | green |
| `agentkit-napi` test binary | NOT LINKED here: the test harness needs node symbols (`napi_*`); lib compiles clean (`cargo check`). Pre-existing environment limit, not a regression — runs green under the node-gyp build. |

Environment notes: the `/proc`-vs-PID-namespace mismatch that blocked the
`/proc` process fixtures in round 1 did not reproduce in this run; those
tests executed green. System-managed policy observation uses the documented
OS locations plus an `AGENTKIT_MANAGED_SETTINGS_SYSTEM_DIRS` injection seam
for hermetic tests.
