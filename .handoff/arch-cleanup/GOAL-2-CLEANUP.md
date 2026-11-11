# GOAL 2 RUNBOOK: continue the architecture cleanup (AgentKit)

You are the coding agent. Goal 1 merged `master` into `arch/cleanup`. This goal makes the gates
truthful, puts the native lane behind ports, removes the extra facade crate and opens the
composition root. Execute work packages WP0 to WP6 in order. Do not skip, reorder or merge them.

Read this whole file before the first command. After every context compaction, re-read section 1,
section 2 and the work package you are in, then read `PROGRESS-2.md`.

- Repository root: the directory that contains this `.handoff/` folder. Paths are relative to it.
- Handoff folder: `H=.handoff/arch-cleanup`. It is untracked and excluded from Git locally (WP0 checks).
  Logs go to `$H/logs/`. Never `git add` anything under `.handoff/`.
- Architecture reference: `$H/agentkit-target-architecture.html`. Sections "Revised target", "Ports",
  "Dependency rules" and "Phases from here" (cards G1, N1, C1) describe the intent of this goal.
- ADRs you must read first: `docs/adr/0021-layered-crate-architecture.md`,
  `docs/adr/0024-repository-ports.md`, `docs/adr/0025-claude-code-native-backend-contracts.md`.

---

## 0. Facts

| Fact | Value |
|---|---|
| Layers | 1 kernel, contracts. 2 core, native, governance. 3 engine, sessions, chat, agents, native-exec. 4 store, providers, toolpacks-coding, lsp, mcp, native-broker, claude, connections. 5 runtime. 6 napi, http, client, agentkit (facade). Dev: testkit. |
| Rule file | `rust/scripts/check-arch.mjs` with `rust/scripts/arch-allowlist.json` |
| State after goal 1 | 24 checked crates, 128 allowlisted entries: 5 `R1b`, 32 `R5 pub use`, 31 `R5 #[path]`, 60 `R5 static` |
| Native executor | `rust/crates/native-exec/src/lib.rs`, about 1.9k lines of source followed by 42 inline tests. It names `SqliteStore` and the concrete `AttemptBroker`. |
| Native broker | `rust/crates/native-broker/src/lib.rs`, built on `agentkit-mcp` and axum |
| Native ports and contracts | `rust/crates/native` (`agentkit-native`), layer 2, no dependency above itself |
| Store methods the executor calls | `bind_native_session`, `observe_native_session`, `current_native_binding`, `invalidate_interactions`, `active_continuation`, `claim_selected_continuation`, `commit_continuation`, `resumable_continuation`, `release_continuation_claim`, `mark_continuation_uncertain` |
| Broker methods the executor calls | `spec()`, `backend().revoke()`, `live_calls()`, `parked_ms()`, `shutdown()` |
| Composition root | `rust/crates/runtime/src/builder.rs`. `build()` registers one executor under `CHAT_TURN_TASK_KIND` and requires a default provider. |

Decision recorded for this goal: the native lane declares all of its ports in `agentkit-native`
(layer 2), next to `NativeBackend`. Reason: the executor's 42 unit tests run against the SQLite store
and the real MCP broker. If the ports lived in `agentkit-native-exec`, the store and the broker would
depend on that crate, and its unit tests would need a dev-dependency cycle that produces duplicate
types (see ADR 0024, consequences). With the ports in `agentkit-native` the tests stay untouched.

---

## 1. Hard rules

1. Never push. Never open, edit or merge a pull request. `git fetch` is allowed.
2. Never commit on `master` or on `arch/cleanup`. Work on the branches named in WP0.
3. Behaviour must not change. These are structural refactors. Every existing test keeps its
   assertions. You may change a test only where a type or import it uses was renamed by this runbook.
4. Never make a gate pass by weakening it: no new `#[ignore]`, no new `#[allow(...)]` (copying an
   attribute that already sits on the method you are wrapping is fine), no deleted test, no edited
   assertion, no relaxed lint.
5. `node scripts/check-arch.mjs --update-allowlist` may be run exactly once, at the step in WP1 that
   says so. Everywhere else the allowlist is edited by hand and may only lose entries.
6. Do not install tools or change toolchain or dependency versions.
7. Stage with `git add -A rust docs CHANGELOG.md`, never at the repository root.
8. Keep output small: redirect cargo and bun output to `$H/logs/` and read summaries with `grep` and `tail`.
9. Stop rule: if the same check fails twice for the same reason after two different fixes, or the
   runbook does not cover the situation, stop. Write what you saw to `PROGRESS-2.md` under "BLOCKED"
   and report. Do not improvise a third approach, and do not start the next work package.
10. Each work package ends with all 16 gates green and a commit. Never start a work package on a red tree.
11. A final answer that lists remaining work means the goal is not complete.

---

## 2. Finish line

Run these from the repository root on the last branch (`arch/c1-open-builder`). Expected results are
in the comments.

```sh
# F1  four stacked branches exist, each contains the previous one
git merge-base --is-ancestor arch/g1-gates arch/n1-native-ports && git merge-base --is-ancestor arch/n1-native-ports arch/n1-fold-facade && git merge-base --is-ancestor arch/n1-fold-facade arch/c1-open-builder && echo STACKED
# F2  all 16 gates passed on the final tree
grep -c '^WP6 gate .* PASS' .handoff/arch-cleanup/PROGRESS-2.md        # 16
# F3  the native executor has no normal dependency on the store, the broker, MCP or axum
(cd rust && cargo tree --locked -p agentkit-native-exec -e normal | grep -cE 'agentkit-(store|native-broker|mcp)|axum|rmcp|rusqlite')   # 0
# F4  the two layer exceptions from the merge are gone
grep -c 'agentkit-native-exec ->' rust/scripts/arch-allowlist.json            # 0
# F5  the facade crate is gone
test ! -e rust/crates/facade && echo FACADE-GONE
# F6  the builder accepts executors per kind and a native lane
rg -n 'pub fn (executor|turn_kinds|native_lane)\(' rust/crates/runtime/src/builder.rs   # three lines
# F7  no test lost, no new ignore (compare with the WP0 baseline numbers in PROGRESS-2.md)
rg -c '^\s*#\[(tokio::)?test' rust -g '*.rs' | awk -F: '{s+=$NF} END{print s}'   # baseline or more
rg -c '^\s*#\[ignore' rust -g '*.rs' | awk -F: '{s+=$NF} END{print s}'            # baseline exactly
# F8  check-arch reports per rule and passes
(cd rust && node scripts/check-arch.mjs)                                       # one "ok" line with a count per rule
# F9  report written
test -s .handoff/arch-cleanup/REPORT-2.md && echo REPORT
```

---

## 3. Working method

- Progress file `$H/PROGRESS-2.md`: one block per work package with `DONE`, the numbers asked for
  and the commit hashes. Read it before every work package.
- Commits: conventional-commit subjects. Inside a work package commit a pure move separately from a
  behaviour-neutral rewrite when both happen.
- The 16 gates are listed in section 5. "Run the gates" means all 16, each into
  `$H/logs/<wp>-<nn>-<name>.log`.
- Compile loop: `cargo check --locked --workspace --all-targets --all-features` into a log, then
  `grep -E '^error' <log> | sort | uniq -c | sort -rn | head -30`, fix one error class, repeat.
- If a manifest changes, run `cargo update --workspace` once and check that
  `git diff -- rust/Cargo.lock` shows no third-party package added, removed or re-versioned.

---

## 4. Work packages

### WP0 Preconditions, base, baseline

```sh
grep -qxF '/.handoff/' .git/info/exclude || echo '/.handoff/' >> .git/info/exclude   # local ignore, not committed
git status --porcelain                                  # nothing
test -s .handoff/arch-cleanup/REPORT-1.md && echo GOAL1-REPORTED
git log -1 --format='%P' arch/cleanup | wc -w           # 2 (a merge commit)
git fetch origin || echo "fetch failed, using local refs"
if git merge-base --is-ancestor arch/cleanup origin/master || git diff --quiet origin/master arch/cleanup; then BASE=origin/master; else BASE=arch/cleanup; fi
echo "BASE=$BASE"
git switch -c arch/g1-gates "$BASE"
df -h . | tail -1                                       # under 30G free: rm -rf rust/target/debug/incremental
```

`BASE` is `origin/master` when PR #4 has landed, otherwise the local `arch/cleanup`. Run the block
above as one shell command so the variable is set when `git switch` uses it. Record the chosen base
and its hash in the progress file. If goal 1 is not reported as complete, stop.

Baseline on this branch: run the 16 gates. Record Rust `passed/failed/ignored`, the test-attribute
count, the ignore count and the `check-arch` line. All 16 must pass before you continue. If one does
not, stop: goal 1 is not finished.

### WP1 (phase G1): gates that measure the right thing

Branch `arch/g1-gates`. Files: `rust/scripts/check-arch.mjs`, `rust/scripts/arch-allowlist.json`,
`rust/deny.toml`, `docs/adr/0021-layered-crate-architecture.md`. No Rust source changes.

**1.1 Remove names that will never exist.** Delete `agentkit-store-sqlite` and
`agentkit-store-legacy-import` from `LAYERS` in `check-arch.mjs` and from the wrapper lists in `deny.toml`.

**1.2 Rewrite the static rule (R5).** Today the script flags any `static` whose type starts with
`Mutex`, `RwLock`, `LazyLock` or `OnceLock`. That reports 34 immutable lazy constants and misses
atomics, `Once`, `Notify`, aliased mutexes and thread-locals. New rule:

- A declaration is a line matching `^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?([A-Z_0-9]+)\s*:`.
  Its type text `T` is the text after that first `:` up to the first `=` or `;`. A type never
  contains `=`. If the first line has neither character, join up to five following lines first.
- Track `thread_local! {` blocks by brace depth. A declaration inside one is always a violation.
- It is a violation when: it is `static mut`; or it is inside `thread_local!`; or `T` matches
  `Mutex|RwLock|Atomic[A-Za-z0-9]*|OnceLock|OnceCell|\bOnce\b|Notify|RefCell|\bCell<|Condvar|Barrier|Semaphore|DashMap`;
  or `T` (whitespace removed, a leading `std::sync::` removed) is `LazyLock<X>` and `X` is not one of
  `Regex`, `regex::Regex`, `Vec<Regex>`, `Vec<regex::Regex>`, `String`, `Vec<String>`,
  `Option<String>`, `HashSet<&'staticstr>`, `BTreeSet<&'staticstr>`.
- Anything else (`&str`, slices, numbers, arrays, whitelisted lazies) is immutable and not reported.
- The violation id stays `R5 <crate> <file> static <NAME>`. `agentkit-sys` stays exempt.

**1.3 Add R11.** For every checked crate in layer 4 or 6 except `agentkit-napi`: a normal (non-dev)
dependency on `agentkit-store` is `R11 <crate> -> agentkit-store`.

**1.4 Add R12.** For every checked crate in layers 1, 2, 3 and 5: a file returned by the existing
`sourceFiles` generator with more than 1000 lines is `R12 <crate> <relative file>`.

**1.5 Add the R8 counter.** In `crates/napi/src` (same file filter): count lines matching
`^\s*#\[napi(\(|\])` that contain neither `catch_unwind` nor `object`. Report one violation
`R8 agentkit-napi exports-without-catch_unwind=<N>`. The number makes the entry stale as soon as
exports gain `catch_unwind`, so it can only be lowered.

**1.6 Report per rule.** The success line becomes, for example:
`check-arch: ok (24 crates; R1b 5, R5-glob 32, R5-path 31, R5-static 42, R8 1, R11 2, R12 8)`.
Count by the id prefix (`R5 ... pub use`, `R5 ... #[path]`, `R5 ... static`).

**1.7 Re-baseline once.** Run `node scripts/check-arch.mjs --update-allowlist`, then compare
`git diff -- rust/scripts/arch-allowlist.json` with this expectation:

- Removed, 29 `R5 static` entries: `agentkit-chat ... BLOCKS`, `agentkit-claude ... GOOD_PATH`, and
  in `agentkit-toolpacks-coding`: `CONFIG_NAMES`, `ENV_ORDER`, `RE`, `ASSIGNMENT`, `DANGEROUS`,
  `DURATION`, `EVAL_SHORT`, `PLAIN_CHARS`, `PYTHON`, `SAFE_SCRIPT`, `SHELL_CODE`, `TOKEN_SPLIT`,
  `RE_DASH`, `RE_QUOTE_DOUBLE`, `RE_QUOTE_SINGLE`, `RE_SPACE`, `AGENT_VARS`, `CREDENTIAL_URL`,
  `SECRET_NAME`, `DRIVER_KEY`, `REV_PATTERN`, `BOOT_ID`, `ID_PATTERN`, `NUMERIC`, `PS_HEAD`,
  `PS_TABLE`, `PS_TOKEN`.
- Added, 11 `R5 static` entries: `agentkit-agents src/advisor/engine.rs ATTEMPTS`,
  `agentkit-chat src/governance/native_command_drain.rs CHANGED`,
  `agentkit-mcp src/manager/session.rs REGISTRATIONS`, `agentkit-store src/faults.rs BOUND`,
  `agentkit-store src/transaction.rs CLEANUP_FAILED`, and in `agentkit-toolpacks-coding`:
  `coordination.rs NEXT`, `coordination.rs LEGACY_SWEEPED`, `coordination.rs WARNED`,
  `process.rs REAP_GATE`, `process.rs HANG_EXIT_OBSERVATION`, `process.rs HARD_SETTLE_TEST_MS`.
- Added, 2 `R11`: `agentkit-http`, `agentkit-mcp`.
- Added, 8 `R12`: `agentkit-agents src/subagents/service.rs`, `agentkit-chat src/chat/history.rs`,
  `agentkit-chat src/governance/durable_execution.rs`, `agentkit-core src/run.rs`,
  `agentkit-engine src/runner.rs`, `agentkit-native-exec src/lib.rs`,
  `agentkit-sessions src/run_profiles.rs`, `agentkit-sessions src/session/mod.rs`.
- Added, 1 `R8` with the measured number.
- Unchanged: all `R1b`, all `R5 pub use`, all `R5 #[path]`.

The expectation was produced by a text scan, so a small difference is possible. For every entry that
differs, open the declaration, decide whether the new rule classifies it correctly, and write one
line per difference into the progress file. If more than six entries differ, your implementation is
wrong: fix it and restore the allowlist with `git checkout -- rust/scripts/arch-allowlist.json`
before running the update again. That repeat counts as the same single use.

**1.8 ADR 0021.** Append a section "Amendment (G1)": reword R5 as "no glob re-exports at crate roots,
no `#[path]`, no process-global mutable state outside `agentkit-sys`; immutable lazy constants are
allowed"; add R11, R12 and R13 with the wording of the reference page; add this table of which phase
removes which group: `R1b` and `R11` by P5e (the two native-exec edges by N1), `R5 static` in
toolpacks-coding process and coordination files by S1, in napi by the NAPI phase, in mcp by P5e,
`R12` by the phase that owns each file, `R8` by the NAPI phase.

Run the gates. Commit: `chore(arch): detect real shared state; add store, file-size and napi rules`.

### WP2 (phase N1, part 1): broker port

```sh
git switch -c arch/n1-native-ports arch/g1-gates
```

**2.1 New file `rust/crates/native/src/broker.rs`.** Move into it from
`rust/crates/native-broker/src/lib.rs`, text unchanged with doc comments: `BrokerToolDefinition`,
`BrokerLimits`, `impl Default for BrokerLimits`, and the constant `BROKER_AUTHORIZE_BUDGET_MS`. Add:

```rust
/// What the native executor needs from a live per-attempt tool broker.
/// The MCP implementation lives in `agentkit-native-broker`.
#[async_trait]
pub trait AttemptBrokerHandle: Send + Sync {
    /// Endpoint the vendor runtime is told to connect to.
    fn spec(&self) -> McpEndpointSpec;
    /// Stop authenticating immediately and cancel live calls.
    fn revoke(&self);
    fn live_calls(&self) -> usize;
    fn parked_ms(&self) -> u64;
    async fn shutdown(&self);
}
```

Export all four items and the trait from `rust/crates/native/src/lib.rs` with explicit names.

**2.2 `agentkit-native-broker`.** Import the moved items from `agentkit_native` and re-export them
with an explicit `pub use agentkit_native::{BROKER_AUTHORIZE_BUDGET_MS, BrokerLimits, BrokerToolDefinition};`
so existing paths keep working. Add `impl AttemptBrokerHandle for AttemptBroker` that forwards to the
existing inherent methods (`revoke` forwards to `self.backend().revoke()`).

**2.3 `agentkit-native-exec`.** Replace every `Arc<AttemptBroker>` with `Arc<dyn AttemptBrokerHandle>`,
including the return type of `AttemptBrokerFactory::start`. Replace `broker.backend().revoke()` with
`broker.revoke()`. Import `BrokerLimits`, `BrokerToolDefinition` and the trait from `agentkit_native`.
In `Cargo.toml` move `agentkit-native-broker` from `[dependencies]` to `[dev-dependencies]` (the
inline tests still use the real broker).

**2.4 Allowlist.** Delete the line `R1b agentkit-native-exec -> agentkit-native-broker` by hand.

Check: `cargo tree --locked -p agentkit-native-exec -e normal` shows no `agentkit-native-broker`,
`agentkit-mcp`, `rmcp` or `axum`. Run the gates. Update the public-API baselines for the three
crates after reading the diff. Commit: `refactor(native): broker port in agentkit-native; executor drops the MCP broker`.

### WP3 (phase N1, part 2): run-repository port and generic executor

Same branch `arch/n1-native-ports`.

**3.1 New file `rust/crates/native/src/repository.rs`.** Move the four records into it, text
unchanged: `NativeSessionBinding` and `BindNativeSession` from `rust/crates/store/src/native_exec.rs`,
`NativeContinuationBinding` and `CommitContinuation` from `rust/crates/store/src/native_continuations.rs`.
Add the trait. Each signature is the store's inherent method with `Result<_, Self::Error>`:

```rust
/// Durable facts the native executor reads and writes: the vendor-session
/// lineage ledger and the scoped continuation ledger. Implemented by the
/// SQLite store. The executor never names a concrete store.
#[async_trait]
pub trait NativeRunRepository: Clone + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn bind_native_session(&self, input: BindNativeSession) -> Result<NativeSessionBinding, Self::Error>;
    async fn observe_native_session(&self, attempt_id: &str, vendor_session_id: &str, now: i64) -> Result<NativeSessionBinding, Self::Error>;
    async fn current_native_binding(&self, task_id: &str) -> Result<Option<NativeSessionBinding>, Self::Error>;
    async fn invalidate_interactions(&self, attempt_id: &str, now: i64) -> Result<usize, Self::Error>;
    async fn active_continuation(&self, conversation: &str, parent: &str, history_position: i64, backend: &str, executable: &str, config_identity: &str, system_prompt_revision: Option<&str>) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn claim_selected_continuation(&self, selected: &NativeContinuationBinding, claimant_task_id: &str, claimant_attempt_id: &str, now: i64) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn commit_continuation(&self, input: CommitContinuation) -> Result<NativeContinuationBinding, Self::Error>;
    async fn resumable_continuation(&self, conversation: &str, parent: &str, history_position: i64, backend: &str, executable: &str, config_identity: &str, system_prompt_revision: Option<&str>, attempt_id: &str) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn release_continuation_claim(&self, binding_id: &str, claimant_attempt_id: &str) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn mark_continuation_uncertain(&self, binding_id: &str, claimant_attempt_id: &str) -> Result<Option<NativeContinuationBinding>, Self::Error>;
}
```

If the inherent methods carry `#[allow(clippy::too_many_arguments)]`, copy that attribute onto the
matching trait methods. Export the records and the trait from `agentkit-native` with explicit names.

**3.2 Store.** In `native_exec.rs` and `native_continuations.rs` import the records from
`agentkit_native`. Remove them from the `pub use` lines in `rust/crates/store/src/lib.rs` and fix
the in-repository users to import from `agentkit_native`. Add `rust/crates/store/src/native_repository.rs`
with `impl agentkit_native::NativeRunRepository for SqliteStore { type Error = StoreError; ... }`.
Each method forwards to the inherent method of the same name, for example
`SqliteStore::bind_native_session(self, input).await`. Do not change the inherent methods or the SQL.

**3.3 Executor becomes generic.** In `rust/crates/native-exec/src/lib.rs`:

- `NativeAgentExecutor<R>`, `ContinuationClaim<R>` and every helper that holds a store take
  `R` instead of `SqliteStore`. The bound, written once as a `where` clause and repeated on each impl:
  `R: agentkit_engine::TaskRepository + agentkit_native::NativeRunRepository<Error = <R as agentkit_engine::TaskRepository>::Error>`.
- `impl<R> TaskExecutor<R> for NativeAgentExecutor<R>` and `NativeDispatchExecutor<R>` with
  `Arc<dyn TaskExecutor<R>>` fields.
- Replace the three private aliases from goal 1 by generic forms: errors are
  `agentkit_engine::EngineError<<R as TaskRepository>::Error>`, the context is
  `agentkit_engine::ExecutionContext<R>`, store calls end in `.map_err(EngineError::Store)?`.
- `AttemptBrokerFactory::start` cannot mention `R`. Change its return type to
  `Result<Arc<dyn AttemptBrokerHandle>, agentkit_engine::ExecutionFailure>`. `EngineError` already
  converts from `ExecutionFailure` with `?`.
- The associated functions `parse_target`, `validate_frozen_semantics`, `validate_frozen_stanza` and
  `capability_verdict` have no `self`, so on a generic type every caller would need a type
  annotation. Delete them. Callers use `agentkit_native::parse_target`,
  `agentkit_native::validate_frozen_semantics`, `agentkit_native::validate_frozen_stanza` and
  `agentkit_native::validate_execution_target`. In the tests this is a text replacement of the call
  path only.
- In the inline tests the only other allowed edits are type annotations such as
  `NativeAgentExecutor::<SqliteStore>::new(...)` where inference needs them, and factory
  implementations returning the new error type.
- `Cargo.toml`: move `agentkit-store` from `[dependencies]` to `[dev-dependencies]`.

**3.4 Allowlist.** Delete `R1b agentkit-native-exec -> agentkit-store` by hand.

**3.5 ADR 0024.** Append "Addendum (N1): native-lane ports" with the decision from section 0 of
this runbook and the two port names.

**3.6 Changelog.** Under `## 0.8.0 — unreleased` add a short paragraph: `NativeAgentExecutor` and
`NativeDispatchExecutor` are generic over the repository; `AttemptBrokerFactory::start` returns
`Arc<dyn AttemptBrokerHandle>` and `ExecutionFailure`; the four native records and both ports live
in `agentkit-native`.

Check: finish-line command F3 prints 0, and the 42 inline tests still exist
(`rg -c '#\[(tokio::)?test' rust/crates/native-exec/src/lib.rs` prints 42). Run the gates. Update
baselines after reading the diff. Commit: `refactor(native): NativeRunRepository port; executor generic over the repository`.

### WP4 (phase N1, part 3): fold `agentkit-facade`

```sh
git switch -c arch/n1-fold-facade arch/n1-native-ports
```

Two crates are called "facade" today. `rust/crates/agentkit` (package `agentkit-rust`) is the SDK
facade and stays. `rust/crates/facade` (package `agentkit-facade`) holds `NativeChat` and a driver
conformance harness and goes away.

1. `git mv rust/crates/facade/src/conformance.rs rust/crates/testkit/src/conformance/native_backend.rs`.
   Register it in `rust/crates/testkit/src/conformance.rs` and add `agentkit-native` and `tokio-util`
   to testkit's dependencies. The file is generic over the ports and must not import `agentkit-claude`.
2. `git mv rust/crates/facade/src/lib.rs rust/crates/agentkit/src/native_chat.rs`. Remove its
   `pub mod conformance;` line. In `rust/crates/agentkit/src/lib.rs` add
   `#[cfg(feature = "claude")] pub mod native_chat;`.
3. `rust/crates/agentkit/Cargo.toml`: add optional dependencies `agentkit-native`, `agentkit-claude`,
   `serde`, `thiserror`, `tokio` (feature `fs`), `tokio-util` (feature `rt`) and the feature
   `claude = ["dep:agentkit-native", "dep:agentkit-claude", "dep:serde", "dep:thiserror", "dep:tokio", "dep:tokio-util"]`.
4. Tests: `git mv` `rust/crates/facade/tests/facade.rs` and `live_claude.rs` to
   `rust/crates/agentkit/tests/` and `conformance.rs` to `rust/crates/agentkit/tests/native_conformance.rs`.
   Declare each as `[[test]]` with `required-features = ["claude"]`. Add `agentkit-testkit` and the
   test-only crates as dev-dependencies. Fix imports: `agentkit_facade::X` becomes
   `agentkit_rust::native_chat::X`, `agentkit_facade::conformance::X` becomes
   `agentkit_testkit::conformance::native_backend::X`.
5. Delete `rust/crates/facade`, its line in `[workspace.dependencies]`, its entry in `LAYERS`, and
   `rust/api/agentkit-facade.txt`. Update `docs/adr/0025-...` addendum table and the ADR 0021
   amendment line that mentions the temporary crate.
6. `rg -n 'agentkit[-_]facade' rust docs CHANGELOG.md` may only show the changelog and ADR history lines.

Run the gates (the test count must not drop: the same tests now run under `--all-features` in other
crates). Commit: `refactor(facade): fold agentkit-facade into agentkit (feature claude) and testkit`.

### WP5 (phase C1): open the composition root

```sh
git switch -c arch/c1-open-builder arch/n1-fold-facade
```

**5.1 Builder API** in `rust/crates/runtime/src/builder.rs`. Add three methods and nothing else to
the public surface:

```rust
/// Additional task kinds served by the turn executor (the chat executor,
/// or the native dispatch wrapper when a native lane is configured).
pub fn turn_kinds(mut self, kinds: Vec<String>) -> Self
/// An executor for a task kind the runtime does not own.
pub fn executor(mut self, kind: impl Into<String>, executor: Arc<dyn TaskExecutor<SqliteStore>>) -> Self
/// Route turns whose frozen execution target is native to a vendor runtime.
pub fn native_lane(mut self, lane: NativeLane) -> Self
```

`NativeLane` is a new public struct in `rust/crates/runtime/src/lanes/native.rs` with public fields
`backend: Arc<dyn NativeBackend>`, `supervisor: Arc<dyn ProcessSupervisor>`,
`locator: Arc<dyn ExecutableLocator>`, `broker_factory: Option<Arc<dyn AttemptBrokerFactory>>`,
`options: NativeExecutorOptions`.

In `build()`, where the executor map is created today:

```rust
let chat: Arc<dyn TaskExecutor<SqliteStore>> = /* the existing ChatExecutor expression */;
let turn: Arc<dyn TaskExecutor<SqliteStore>> = match native_lane {
    Some(lane) => Arc::new(NativeDispatchExecutor {
        native: Arc::new(NativeAgentExecutor::new(store.clone(), lane.backend, lane.supervisor, lane.locator, lane.broker_factory, lane.options)),
        fallback: chat,
    }),
    None => chat,
};
let mut executors = HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), Arc::clone(&turn))]);
for kind in turn_kinds { executors.insert(kind, Arc::clone(&turn)); }
for (kind, executor) in extra_executors {
    if executors.insert(kind.clone(), executor).is_some() {
        return Err(RuntimeError::invalid(format!("executor kind registered twice: {kind}")));
    }
}
```

Without `native_lane`, `turn_kinds` and `executor` the map must be exactly what it is today. Keep the
load-bearing order described at the top of the file. Add `agentkit-native` and `agentkit-native-exec`
to the runtime's dependencies. Do not add `agentkit-claude`: the consumer passes the backend.

**5.2 Test** `rust/crates/runtime/tests/native_lane.rs`. Use the scripted native ports from
`rust/crates/native-exec/examples/native-chat-lifecycle.rs` (copy them into the test file) and the
`Fake` provider from `rust/crates/runtime/tests/two_runtimes.rs`. Two tests:

- A runtime built with `.turn_kinds(vec!["consumer.chat".into()]).native_lane(...)`: submit a task of
  kind `consumer.chat` whose payload carries the native `executionTarget` and `turnInput`, exactly as
  the example submits its task, through `runtime.host()`. It must reach a completed state and the
  scripted backend must have been called.
- `.executor("chat.turn", ...)` makes `build()` return the duplicate-kind error.

The existing `two_runtimes.rs` test already proves that a runtime built without the new calls
behaves as before. Do not change it.

**5.3 Facade features** in `rust/crates/agentkit`. Rename the feature `runtime` to `core` (it only
enables `agentkit-core` and `agentkit-connections`) and update the features that list it. Add
`runtime = ["durable", "dep:agentkit-runtime"]` and `#[cfg(feature = "runtime")] pub use agentkit_runtime as runtime;`.
Search for users of the old feature name: `rg -n 'features = \[[^]]*"runtime"' rust examples packages`.

**5.4 ADR 0026** `docs/adr/0026-execution-owners-and-open-composition-root.md`: context (one
execution owner per admitted turn, the builder used to hard-wire one executor), decision (the three
builder methods, `NativeLane`, the driver stays outside the runtime, facade features), consequences,
and one open question: whether a host with only a native lane may omit the default provider.

**5.5 Changelog** paragraph for 0.8.0 naming the three builder methods and the feature rename.

Run the gates. Update baselines after reading the diff. Commit:
`feat(runtime): executors per task kind and an optional native lane on the builder`.

### WP6 Final verification and report

On `arch/c1-open-builder` with a clean tree: run all 16 gates once more and write one line per gate
to the progress file in exactly this form: `WP6 gate 03 cargo test: PASS`. Then run every command of
section 2 and write `$H/REPORT-2.md`:

```
# Goal 2 report
- Base used: <origin/master | arch/cleanup> at <hash>
- Branches: arch/g1-gates <hash>, arch/n1-native-ports <hash>, arch/n1-fold-facade <hash>, arch/c1-open-builder <hash>
- Finish line: F1..F9 with the command output in one line each
- check-arch: before <line>, after <line>
- Allowlist: entries removed <n>, added by the WP1 re-baseline <n>, differences from the expectation <list>
- Rust tests: baseline <p>/<f>/<i>, final <p>/<f>/<i>
- Public API: crates changed <list> with one line each
- Deviations from the runbook: <none | list with reason>
- For the owner: four stacked branches to review and push; examples to port (AGENTKIT-132); next phase S1 (agentkit-sys)
```

---

## 5. The 16 gates

| # | Gate | Directory |
|---|---|---|
| 01 | `cargo fmt --all --check` | `rust/` |
| 02 | `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | `rust/` |
| 03 | `cargo test --locked --workspace --all-features --no-fail-fast` | `rust/` |
| 04 | `cargo machete` | `rust/` |
| 05 | `cargo deny check` | `rust/` |
| 06 | `node scripts/check-arch.mjs` | `rust/` |
| 07 | `node scripts/public-api.mjs` (use `--update` only after reading the diff) | `rust/` |
| 08 | `bun run build:native` | `packages/native/` |
| 09 | `node scripts/node-smoke.mjs` | root |
| 10 | `bun run build:fixture` then `bun test --timeout 60000 packages/client/tests packages/react/tests` | root |
| 11 | `AGENTKIT_REQUIRE_NATIVE=1 bun test --timeout 60000 packages/native packages/core` | root |
| 12 | `bun run typecheck` | root |
| 13 | `bun run lint` | root |
| 14 | `bun run build` | root |
| 15 | `bun run check:api` | root |
| 16 | `bun run contracts:check` | root |

Gate 03 passes when `failed` equals the WP0 baseline with the same test names and `passed` is not
lower than the baseline. Sum the `test result:` lines:
`grep -E '^test result:' <log> | awk '{p+=$4; f+=$6; i+=$8} END {print p, f, i}'`.

---

## 6. Not in this goal

Do not start any of these, even if you have budget left. Each is its own goal:

- S1: `agentkit-sys` (owned processes, private files, all `unsafe`), Windows compile gate.
- P5e: repository ports for sessions, chat, agents and MCP configuration; in-memory twins and
  conformance suites, including one for `NativeRunRepository`; `SessionsError::InputConflict`.
- GOV: moving `chat::governance` into its own context.
- P8b, CT, NAPI: toolpacks internals, contract tooling, binding slimming.
- Porting the `native-chat-lifecycle` and `teammate-consumer` examples onto the builder (AGENTKIT-132).
