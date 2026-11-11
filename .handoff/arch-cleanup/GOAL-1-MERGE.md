# GOAL 1 RUNBOOK: merge `master` into `arch/cleanup` (AgentKit)

You are the coding agent. Execute stages S0 to S10 in order. Do not skip, reorder or merge stages.
Read this whole file before the first command. After every context compaction, re-read section 1,
section 2 and the stage you are in, then read `PROGRESS-1.md`.

- Repository root: the directory that contains this `.handoff/` folder. All paths below are relative to it.
- Handoff folder: `H=.handoff/arch-cleanup`. It is untracked; S0 excludes it from Git locally. Logs go to `$H/logs/`.
  Never `git add` anything under `.handoff/`.
- Architecture reference: `$H/agentkit-target-architecture.html`, sections "Merging master" and "Revised target".
  Open it only when a step says so. This runbook is self-sufficient.

---

## 0. Facts you must not re-derive

They were verified on 2026-11-11 with a trial merge. Trust them unless a command proves otherwise.

| Fact | Value |
|---|---|
| `arch/cleanup` tip (head of PR #4) | `4a6342f87141440ad291acb196e7d2b44dc6e4ae` |
| `master` tip | `755df64d2f248abc74d5253623abfb30437ccd99` |
| Merge base | `7aed700` |
| What `arch/cleanup` did | Deleted `rust/crates/host`. Its code now lives in `engine`, `sessions`, `chat`, `agents`, `governance`, `runtime`, `http`, `providers`. `HostError`/`SessionError` no longer exist: use `EngineError<StoreError>`, `SessionsError`, `ChatError`. |
| What `master` added | The Claude Code native backend: new crates `native`, `claude`, `facade`; new files `host/src/native_executor.rs`, `host/src/native_bridge.rs`; store files `native_exec.rs`, `native_continuations.rs` (schema 16 to 19); `frozen_payload` on session inputs. |
| Conflicted paths in the merge | 14 (listed in S2) |
| Test attributes under `rust/` | 1840 on `arch/cleanup`, +273 from `master`, so at least 2113 after the merge |
| `#[ignore]` attributes under `rust/` | 8 on `arch/cleanup`, +4 from `master`, so exactly 12 |
| Rust tests on `arch/cleanup` | 1826 passed, 0 failed, 8 ignored (from the commit message; S1 measures it) |

Target placement of master's native code (decided by the owner, do not change it):

| Code from master | New home | Crate name | Layer |
|---|---|---|---|
| `crates/native` | same path | `agentkit-native` | 2 |
| pure contract parts of `native_executor.rs` | `rust/crates/native/src/frozen.rs` | `agentkit-native` | 2 |
| `host/src/native_executor.rs` | `rust/crates/native-exec/src/lib.rs` | `agentkit-native-exec` | 3 |
| `host/src/native_bridge.rs` | `rust/crates/native-broker/src/lib.rs` | `agentkit-native-broker` | 4 |
| `crates/claude` | same path | `agentkit-claude` | 4 |
| `crates/facade` | same path (folded away in goal 2) | `agentkit-facade` | 6 |

---

## 1. Hard rules

1. Never push. Never touch PR #4. Never commit on `master`. Never move `arch/cleanup` before S10.
2. All work happens on the branch `integration/master-merge`. Never delete that branch.
3. Do not change behaviour. This goal moves code, fixes imports and types, and ports master's session
   changes to the typed errors. It does not redesign anything.
4. Never make a gate pass by weakening it: no new `#[ignore]`, no `#[allow(...)]`, no deleted or
   commented-out test, no edited assertion, no `--update-allowlist`, no relaxed lint.
   One translation is allowed in tests: `error.code` becomes `error.failure_event_code()`.
5. The architecture allowlist may grow by exactly the four entries in Reference D. Nothing else.
6. Do not install tools or change toolchain versions. If a tool is missing, stop and report.
7. Do not run `git add -A` at the repository root. Use `git add -A rust docs CHANGELOG.md`.
8. Keep output small. Redirect every cargo and bun command to a log file and read only what you need:
   `grep`, `tail -40`, error summaries. Never print a full test or build log.
9. Stop rule: if the same check fails twice for the same reason after two different fixes, or this
   runbook does not cover the situation, stop. Write what you saw to `PROGRESS-1.md` under "BLOCKED"
   and report. Do not improvise a third approach.
10. A final answer that lists remaining work means the goal is not complete.

---

## 2. Finish line

The goal is complete only when every condition F1 to F10 is true. Verify them with these commands,
run from the repository root on branch `arch/cleanup` after S10. The expected result is in the comment.

```sh
# F1  arch/cleanup is ONE merge commit on top of 4a6342f and the master tip
git log -1 --format='%P' arch/cleanup          # 4a6342f87141440ad291acb196e7d2b44dc6e4ae <master tip>
# F2  its tree equals the integration branch
git diff --quiet integration/master-merge arch/cleanup && echo SAME          # SAME
# F3  all 16 gates passed on that tree
ls .handoff/arch-cleanup/logs/s9-*.log | wc -l                        # 16 or more
grep -c '^S9 gate .* PASS' .handoff/arch-cleanup/PROGRESS-1.md        # 16
# F4  no test lost
rg -c '^\s*#\[(tokio::)?test' rust -g '*.rs' | awk -F: '{s+=$NF} END{print s}'   # 2113 or more
# F5  no new ignores
rg -c '^\s*#\[ignore' rust -g '*.rs' | awk -F: '{s+=$NF} END{print s}'            # 12
# F6  the allowlist grew by exactly four entries
git diff 4a6342f arch/cleanup -- rust/scripts/arch-allowlist.json | grep -c '^+ '   # 4
git diff 4a6342f arch/cleanup -- rust/scripts/arch-allowlist.json | grep -c '^- '   # 0
# F7  no feature-gated validation left
rg -n 'feature = "http"' rust/crates/sessions ; echo "exit=$?"                # no lines, exit=1
# F8  no leftovers
test ! -e rust/crates/host && echo HOST-GONE                                  # HOST-GONE
git grep -nE '^(<<<<<<< |>>>>>>> )' arch/cleanup -- rust docs ; echo "exit=$?" # no lines, exit=1
# F9  schema version
rg -n 'pub const SCHEMA_VERSION: i64 = 19' rust/crates/store/src/schema.rs    # one line
# F10 report written
test -s .handoff/arch-cleanup/REPORT-1.md && echo REPORT               # REPORT
```

---

## 3. Working method

- Progress file: `$H/PROGRESS-1.md`. After each stage append one block:
  `## S<n> <name>: DONE` plus the numbers the stage asked for and the commit hash. Before each stage,
  read the file to see where you are. If it says a stage is DONE, do not redo it.
- Commits on the integration branch: one or more per stage, conventional-commit subjects such as
  `wip(merge): S3 compile fixes`. They are working commits. S10 folds them into one merge commit.
- Cargo commands run from `rust/`. Bun and node commands run from the repository root unless stated.
- Log helper pattern (always use it):
  ```sh
  cd rust && cargo check --locked --workspace --all-targets --all-features > ../$H/logs/s3-check.log 2>&1; echo "exit=$?"
  grep -cE '^error' ../$H/logs/s3-check.log
  grep -E '^error(\[E[0-9]+\])?' ../$H/logs/s3-check.log | sort | uniq -c | sort -rn | head -30
  grep -n -A14 -E '^error' ../$H/logs/s3-check.log | head -150
  ```
- Fix errors class by class using Reference C. Re-run the check after each class.

---

## 4. Stages

### S0 Preflight

```sh
grep -qxF '/.handoff/' .git/info/exclude || echo '/.handoff/' >> .git/info/exclude   # local ignore, not committed
git status --porcelain                 # must print nothing
git rev-parse arch/cleanup master      # compare with section 0
git worktree list                      # arch/cleanup must not be checked out in another worktree
cargo --version && rustc --version && bun --version && node --version
cargo machete --version && cargo deny --version && cargo public-api --version
df -h . | tail -1                      # if less than 30G free: rm -rf rust/target/debug/incremental
ls rust/crates/.claude 2>/dev/null     # if it exists, delete it: it breaks the crates/* glob
mkdir -p .handoff/arch-cleanup/logs
```

- `arch/cleanup` differs from section 0: stop and report.
- `master` is ahead of `755df64`: continue. In S2 you may see extra conflicts. Resolve them with the
  principle "master's logic, arch/cleanup's structure and type names" and record each one.
- A tool is missing: stop and report.

Create `$H/PROGRESS-1.md` with the heading `# Goal 1 progress` and the S0 block.

### S1 Baseline on arch/cleanup

```sh
git switch arch/cleanup
cd rust
cargo test --locked --workspace --all-features --no-fail-fast > ../$H/logs/s1-test.log 2>&1; echo "exit=$?"
grep -E '^test result:' ../$H/logs/s1-test.log | awk '{p+=$4; f+=$6; i+=$8} END {print "passed="p, "failed="f, "ignored="i}'
grep -E '^test .* FAILED$|^---- .* stdout ----$' ../$H/logs/s1-test.log | sort -u | head -40
node scripts/check-arch.mjs            # expect: check-arch: ok (19 crates, 124 allowlisted violations)
cd ..
```

Record `passed`, `failed`, `ignored` and the names of any failing tests as `BASELINE` in the progress
file. Baseline failures are pre-existing: do not fix them, and they must not grow. Expected: 1826 / 0 / 8.

### S2 Integration branch, conflict resolution, relocation (one commit, may not compile yet)

```sh
git switch -c integration/master-merge arch/cleanup
git merge --no-ff --no-commit master        # exits non-zero, that is expected
git status --short | grep -E '^(UU|AA|DU|UD|UA|AU|DD) '
```

Expected 14 paths:

```
UU rust/Cargo.lock
UU rust/crates/agents/src/advisor/engine.rs
UA rust/crates/chat/src/native_bridge.rs
UA rust/crates/chat/src/native_executor.rs
UA rust/crates/chat/tests/live_claude_mcp.rs
UA rust/crates/chat/tests/native_admission_round4.rs
UA rust/crates/chat/tests/native_session_lane.rs
DU rust/crates/host/Cargo.toml
DU rust/crates/host/src/lib.rs
DU rust/crates/host/src/ports.rs
UA rust/crates/runtime/examples/native-chat-lifecycle.rs
UU rust/crates/sessions/src/session/mod.rs
UU rust/crates/store/Cargo.toml
UU rust/crates/store/src/lib.rs
```

Now do steps 2.1 to 2.9 in this order.

**2.1 Lockfile and host leftovers**

```sh
git checkout --ours rust/Cargo.lock
git rm -q -f rust/crates/host/Cargo.toml rust/crates/host/src/lib.rs rust/crates/host/src/ports.rs
rm -rf rust/crates/host
```

Master's versions of the three host files stay readable with `git show master:rust/crates/host/src/ports.rs`.

**2.2 Move the files Git placed in the wrong crates**

```sh
mkdir -p rust/crates/native-exec/src rust/crates/native-exec/examples rust/crates/native-broker/src rust/crates/native-broker/tests
mv rust/crates/chat/src/native_executor.rs               rust/crates/native-exec/src/lib.rs
mv rust/crates/chat/src/native_bridge.rs                 rust/crates/native-broker/src/lib.rs
mv rust/crates/chat/tests/live_claude_mcp.rs             rust/crates/native-broker/tests/live_claude_mcp.rs
mv rust/crates/chat/tests/native_session_lane.rs         rust/crates/sessions/tests/native_session_lane.rs
mv rust/crates/chat/tests/native_admission_round4.rs     rust/crates/sessions/tests/native_admission_round4.rs
mv rust/crates/runtime/examples/native-chat-lifecycle.rs rust/crates/native-exec/examples/native-chat-lifecycle.rs
git mv docs/adr/0021-claude-code-native-backend-contracts.md docs/adr/0025-claude-code-native-backend-contracts.md
```

Do not edit the content of the moved files in this stage, except what 2.4 to 2.8 name.

**2.3 Manifests**: create and edit the files exactly as Reference A says.

**2.4 `rust/crates/agents/src/advisor/engine.rs`**: one conflict. Result:

```rust
                    &agentkit_sessions::SteerInput {
                        frozen_payload: None,
                        content: Value::String(note),
```

**2.5 `rust/crates/store/Cargo.toml`**: one conflict in `[dependencies]`. Keep the `HEAD` lines and
add one line. Delete master's three lines. Result:

```toml
agentkit-contracts = { workspace = true }
agentkit-engine = { workspace = true }
agentkit-kernel = { workspace = true }
agentkit-native = { workspace = true }
async-trait.workspace = true
chrono = { workspace = true, features = ["std"] }
```

**2.6 `rust/crates/store/src/lib.rs`**: two conflicts. There is no `models` module on this branch
(those records live in `agentkit-engine`). Results:

```rust
mod managed_content;
mod native_continuations;
mod native_exec;
mod proposals;
```

```rust
pub use managed_content::*;
pub use native_continuations::{CommitContinuation, NativeCallReceipt, NativeContinuationBinding};
pub use native_exec::{BindNativeSession, InteractionRow, NativeSessionBinding};
pub use proposals::*;
```

**2.7 `rust/crates/sessions/src/session/mod.rs`**: seven conflicts. One principle for all of them:
take master's parameters and statements, write them with this branch's type names.

| In the conflict | Write |
|---|---|
| `SessionResult<...>` | `SessionsResult<...>` |
| `SessionError::invalid_input(...)` | `SessionsError::invalid_input(...)` |
| extra parameters `frozen: Option<&Map<String, Value>>` and `kind: QueueKind` | keep them |
| `self.check_frozen(frozen, descriptor)?;` and the `let frozen = content.get("frozen")...` block | keep them |
| the big hunk from `fn hash_of` to the end of `fn check_frozen` | take master's block whole, then apply the two renames above |
| the hunk with `Err(error) if error.code == "chat_busy" => {` | see the code block below |

The busy-chat hunk keeps this branch's typed pattern and master's six-argument call:

```rust
            Err(SessionsError::ChatBusy(_))
            | Err(SessionsError::SessionStore(crate::SessionStoreError(
                agentkit_store::StoreError::ScopeBusy(_),
            ))) => {
                self.enqueue_input(session_id, descriptor, kind, content, intent, frozen)
                    .await
            }
```

After resolving, fix the two places that merged without a conflict marker:

- `SessionError::new("input_conflict", format!(...))` in `prior_receipt` becomes
  `SessionsError::from(agentkit_store::StoreError::Duplicate(format!(...)))` with the same message.
  This reuses the existing typed path that already maps to the code `input_conflict` in every transport.
- Both `#[cfg(feature = "http")]` sites: do them in S3 with Reference B. Leave them for now.

```sh
rg -n 'SessionError\b|SessionResult\b' rust/crates/sessions/src     # must print nothing
```

**2.8 Engine**: apply Reference E (two trait methods and `lease_fence`). Then check the call site that
Git merged for you:

```sh
rg -n 'handles_verification_for\(&context.task.payload\)' rust/crates/engine/src/runner.rs   # one line
```

**2.9 Architecture gate inputs and the merge commit**: apply Reference D. Then:

```sh
git grep -nE '^(<<<<<<< |>>>>>>> )' -- rust docs ; echo "markers-exit=$?"     # no lines, exit 1
git add -A rust docs                                                          # also records the six moves
git diff --name-only --diff-filter=U | wc -l                                  # 0
git commit -m "wip(merge): S2 resolve conflicts and relocate the native lane"
git log -1 --format='%h parents: %p'                                          # two parents
git diff -M --name-status master HEAD -- rust/crates | grep -E '^R100.*(native-exec|native-broker|sessions/tests/native_)'
```

The last command must show six `R100` lines. That proves the six files were moved without edits.
Record the commit hash.

### S3 Make the workspace compile

1. Apply Reference B (frozen contract into `agentkit-native`, unconditional validation in sessions).
2. Add the private aliases from Reference C to the top of `rust/crates/native-exec/src/lib.rs`.
3. Let cargo add the new crates to the lockfile. This is the only command in the whole goal without `--locked`:
   ```sh
   cd rust && cargo update --workspace > ../$H/logs/s3-lock.log 2>&1; echo "exit=$?"; cd ..
   git diff arch/cleanup -- rust/Cargo.lock | grep -E '^[+-]name = ' | sort | uniq -c
   ```
   Only `+name = "agentkit-claude"`, `agentkit-facade`, `agentkit-native`, `agentkit-native-broker`,
   `agentkit-native-exec` may appear. If any third-party package was added, removed or changed version,
   a manifest pins something that the workspace does not: fix the manifest (Reference A), restore the
   lockfile with `git checkout arch/cleanup -- rust/Cargo.lock`, and repeat.
4. Loop: `cargo check --locked --workspace --all-targets --all-features` with the log helper, fix by
   error class with Reference C, repeat until `exit=0`.
5. `rg -n 'feature = "http"' rust/crates/sessions` prints nothing.

Commit: `wip(merge): S3 workspace compiles`. Record the number of check runs it took.

### S4 Formatting and lints

```sh
cd rust
cargo fmt --all
cargo fmt --all --check > ../$H/logs/s4-fmt.log 2>&1; echo "exit=$?"
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings > ../$H/logs/s4-clippy.log 2>&1; echo "exit=$?"
grep -E '^(error|warning)' ../$H/logs/s4-clippy.log | sort | uniq -c | sort -rn | head -30
cd ..
```

Master's three crates now inherit the workspace lints for the first time. Fix each finding with the
smallest mechanical change (for example an elided lifetime written out). No logic changes, no `#[allow]`.
For these five crates only (`agentkit-native`, `agentkit-claude`, `agentkit-facade`,
`agentkit-native-exec`, `agentkit-native-broker`) you may let the tool apply its own suggestions:
`cargo clippy --fix --allow-dirty --locked -p <crate> --all-targets`. Read `git diff --stat` afterwards
and reject anything that is not a lint fix.

Commit: `wip(merge): S4 fmt and clippy clean`.

### S5 Tests

```sh
cd rust
cargo test --locked --workspace --all-features --no-fail-fast > ../$H/logs/s5-test.log 2>&1; echo "exit=$?"
grep -E '^test result:' ../$H/logs/s5-test.log | awk '{p+=$4; f+=$6; i+=$8} END {print "passed="p, "failed="f, "ignored="i}'
grep -E '^test .* FAILED$' ../$H/logs/s5-test.log | sort -u
cd ..
rg -c '^\s*#\[(tokio::)?test' rust -g '*.rs' | awk -F: '{s+=$NF} END{print s}'    # 2113 or more
rg -c '^\s*#\[ignore' rust -g '*.rs' | awk -F: '{s+=$NF} END{print s}'             # 12
```

Pass condition: `failed` equals the baseline `failed` with the same test names, and `passed` is at
least baseline `passed` + 250. (Master added 273 test attributes; a few are compiled only on some
platforms.) If `passed` is lower, a test target is not being built: find which with
`grep -c 'Running' $H/logs/s5-test.log` and the list of test binaries, and fix the manifest.

A test fails only after the merge:

1. Read the failure. If it is an import, a type name or `error.code`, apply Reference C.
2. If it is behaviour, the port is wrong. Compare your version with master:
   `git show master:rust/crates/host/src/...` and restore master's logic. Do not touch the assertion.
3. If it still fails after two different fixes, apply the stop rule.

Commit: `wip(merge): S5 tests green`. Record the three numbers.

### S6 Remaining Rust gates

```sh
cd rust
cargo machete > ../$H/logs/s6-machete.log 2>&1; echo "machete=$?"
cargo deny check > ../$H/logs/s6-deny.log 2>&1; echo "deny=$?"
node scripts/check-arch.mjs > ../$H/logs/s6-arch.log 2>&1; echo "arch=$?"; cat ../$H/logs/s6-arch.log | tail -15
node scripts/public-api.mjs > ../$H/logs/s6-api.log 2>&1; echo "api=$?"; tail -60 ../$H/logs/s6-api.log
cd ..
```

- machete reports an unused dependency in a crate you touched: remove that line from its manifest and
  re-run S3 step 4 once.
- deny fails on a layer ban: the only expected change is in Reference D. Anything else: stop rule.
- check-arch expected: `check-arch: ok (24 crates, 128 allowlisted violations)`. A violation that is
  not in Reference D means a step above was done differently. Find it and fix the code or manifest.
  Never add another entry.
- public-api reports differences. Expected: five new crates and changed surfaces in `agentkit-engine`
  (two trait methods, `lease_fence`), `agentkit-sessions` (`frozen_payload` fields), `agentkit-store`
  (native methods and records), `agentkit-mcp` (`RpcId`, `ErrorData`, `RmcpError`), `agentkit-native`
  (frozen contract). If the differences are only these, run `node scripts/public-api.mjs --update`
  and re-run without `--update`. Any other crate changing its API: stop rule. Write a five-line
  summary of the API diff into the progress file.

Commit: `wip(merge): S6 arch, deny, machete and api baselines`.

### S7 Node and TypeScript gates

```sh
(cd packages/native && bun run build:native) > $H/logs/s7-build-native.log 2>&1; echo "build-native=$?"
node scripts/node-smoke.mjs > $H/logs/s7-smoke.log 2>&1; echo "smoke=$?"
bun run build:fixture > $H/logs/s7-fixture.log 2>&1; echo "fixture=$?"
bun test --timeout 60000 packages/client/tests packages/react/tests > $H/logs/s7-client-react.log 2>&1; echo "client-react=$?"
AGENTKIT_REQUIRE_NATIVE=1 bun test --timeout 60000 packages/native packages/core > $H/logs/s7-native-core.log 2>&1; echo "native-core=$?"
bun run typecheck > $H/logs/s7-typecheck.log 2>&1; echo "typecheck=$?"
bun run lint > $H/logs/s7-lint.log 2>&1; echo "lint=$?"
bun run build > $H/logs/s7-build.log 2>&1; echo "build=$?"
bun run check:api > $H/logs/s7-check-api.log 2>&1; echo "check-api=$?"
bun run contracts:check > $H/logs/s7-contracts.log 2>&1; echo "contracts=$?"
git status --porcelain
```

The merge changes nothing under `packages/` or `scripts/`. A failure here therefore comes from the
Rust binding: look at `rust/crates/napi/src/service.rs` (master added `frozen_payload` parsing there).
If `git status` shows changed generated files under `packages/native`, read the diff. Commit them
only if the diff is explained by master's napi change. Otherwise apply the stop rule.

Expected totals from the last green run on `arch/cleanup`: client and React 246 passed, native and
core 750 passed.

### S8 Documentation

Apply Reference F. Then:

```sh
rg -n 'agentkit[-_]host' rust/crates rust/examples docs/adr/0025-claude-code-native-backend-contracts.md | grep -v 'runner-faults.md'
```

Allowed remaining hits: `rust/crates/agents/src/advisor/watchdog.rs`, `rust/crates/napi/src/store_settings.rs`
(both existed before the merge) and the "Addendum" table of ADR 0025.

Commit: `docs(merge): place the native lane in the layered architecture`.

### S9 Full gate run on the final tree

The tree must be clean (`git status --porcelain` prints nothing). Run all 16 gates again, in this
order, each into `$H/logs/s9-<nn>-<name>.log`, and write one line per gate into the progress file in
exactly this form: `S9 gate 03 cargo test: PASS` (or `FAIL`).

| # | Gate | Directory |
|---|---|---|
| 01 | `cargo fmt --all --check` | `rust/` |
| 02 | `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` | `rust/` |
| 03 | `cargo test --locked --workspace --all-features --no-fail-fast` | `rust/` |
| 04 | `cargo machete` | `rust/` |
| 05 | `cargo deny check` | `rust/` |
| 06 | `node scripts/check-arch.mjs` | `rust/` |
| 07 | `node scripts/public-api.mjs` | `rust/` |
| 08 | `bun run build:native` | `packages/native/` |
| 09 | `node scripts/node-smoke.mjs` | root |
| 10 | `bun run build:fixture` then `bun test --timeout 60000 packages/client/tests packages/react/tests` | root |
| 11 | `AGENTKIT_REQUIRE_NATIVE=1 bun test --timeout 60000 packages/native packages/core` | root |
| 12 | `bun run typecheck` | root |
| 13 | `bun run lint` | root |
| 14 | `bun run build` | root |
| 15 | `bun run check:api` | root |
| 16 | `bun run contracts:check` | root |

Any FAIL: fix, commit, and restart S9 from gate 01. Gate 03 passes when it meets the S5 pass condition.

### S10 Fold into one merge commit on arch/cleanup and report

Write the commit message to `$H/merge-message.txt`. Fill in the numbers:

```
merge: master into arch/cleanup (Claude Code native backend onto the layered crates)

Brings AGENTKIT-107 to 118 (master b80b997..<master tip>) onto the ADR 0021 layout.
Behaviour is unchanged; crate paths moved (ADR 0025, renumbered from master's 0021).

- host/src/native_executor.rs -> crates/native-exec (agentkit-native-exec, layer 3)
- host/src/native_bridge.rs   -> crates/native-broker (agentkit-native-broker, layer 4)
- frozen admission contract   -> agentkit-native (layer 2); sessions validate it unconditionally
- session changes ported to SessionsError; engine gains two TaskExecutor methods and lease_fence()
- native, claude and facade crates inherit workspace dependencies and lints

Allowlist additions (ADR 0021 amendment; removed by phase N1 or G1):
  R1b agentkit-native-exec -> agentkit-native-broker
  R1b agentkit-native-exec -> agentkit-store
  R5 agentkit-claude src/backend.rs static GOOD_PATH
  R5 agentkit-claude src/discovery.rs static RECORDED_QUALIFICATIONS

Gates: 16/16 pass. Rust: <passed> passed, <failed> failed, <ignored> ignored.
Client/React: <n> passed. Native/core: <n> passed.
```

```sh
test -z "$(git status --porcelain)" && git rev-parse --abbrev-ref HEAD       # integration/master-merge
git rev-parse arch/cleanup                                                    # still 4a6342f8...
TREE=$(git rev-parse 'HEAD^{tree}')
NEW=$(git commit-tree "$TREE" -p arch/cleanup -p master -F .handoff/arch-cleanup/merge-message.txt)
git branch -f arch/cleanup "$NEW"
git diff --quiet integration/master-merge arch/cleanup && echo SAME
git log -1 --format='%H%n%P%n%s' arch/cleanup
git switch arch/cleanup && git status -sb | head -1
```

If `git config commit.gpgsign` prints `true`, add `-S` to the `commit-tree` command.

Then verify every line of section 2, write `$H/REPORT-1.md` (section 5), and give your final answer.
Do not push. The owner pushes `arch/cleanup` and lands PR #4.

---

## 5. Final report template (`$H/REPORT-1.md`)

```
# Goal 1 report
- arch/cleanup: <new hash>, parents <p1> <p2>
- integration branch: integration/master-merge at <hash> (kept)
- Finish line: F1..F10 each with the command output in one line
- Rust tests: baseline <p>/<f>/<i>, after merge <p>/<f>/<i>
- Test attributes: <n>, ignores: <n>
- check-arch: <summary line>
- Public API: crates added <list>, crates changed <list>
- Extra conflicts beyond the 14 expected: <none | list with resolution>
- Deviations from the runbook: <none | list with reason>
- For the owner: push arch/cleanup, re-run the ignored live Claude tests, land PR #4
```

---

## Reference A: manifests

**A1. `rust/Cargo.toml`**: in `[workspace.dependencies]`, after the `agentkit-rust` line, add:

```toml
agentkit-native = { path = "crates/native", version = "0.1.0" }
agentkit-native-exec = { path = "crates/native-exec", version = "0.1.0" }
agentkit-native-broker = { path = "crates/native-broker", version = "0.1.0" }
agentkit-claude = { path = "crates/claude", version = "0.1.0" }
agentkit-facade = { path = "crates/facade", version = "0.1.0" }
```

**A2. New file `rust/crates/native-exec/Cargo.toml`**

```toml
[package]
name = "agentkit-native-exec"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true
description = "AgentKit native-turn context: durable executor for vendor agent runtimes, one owned process per attempt"
repository = "https://github.com/andrejvysny/AgentKit"
readme = "../../../README.md"

[lints]
workspace = true

[dependencies]
agentkit-core = { workspace = true }
agentkit-engine = { workspace = true }
agentkit-native = { workspace = true }
# Transitional (ADR 0021 amendment): removed by phase N1.
agentkit-native-broker = { workspace = true }
agentkit-store = { workspace = true }
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-util = { workspace = true, features = ["rt"] }

[dev-dependencies]
agentkit-mcp = { workspace = true }
```

**A3. New file `rust/crates/native-broker/Cargo.toml`**

```toml
[package]
name = "agentkit-native-broker"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
publish.workspace = true
description = "AgentKit native tool broker: one governed loopback MCP endpoint per live native attempt"
repository = "https://github.com/andrejvysny/AgentKit"
readme = "../../../README.md"

[lints]
workspace = true

[dependencies]
agentkit-mcp = { workspace = true }
agentkit-native = { workspace = true }
async-trait.workspace = true
axum.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-util = { workspace = true, features = ["rt"] }
uuid.workspace = true

[dev-dependencies]
agentkit-claude = { workspace = true }
reqwest.workspace = true
tempfile.workspace = true
```

**A4. `rust/crates/native/Cargo.toml`**: keep `name` and `description`. Add the `repository` and
`readme` lines and the `[lints]` table as in A2. Replace `[dependencies]` with:

```toml
[dependencies]
async-trait.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
tokio.workspace = true
tokio-util = { workspace = true, features = ["rt"] }
```

**A5. `rust/crates/claude/Cargo.toml`**: same header treatment. Replace the dependency tables with:

```toml
[dependencies]
agentkit-native = { workspace = true }
async-trait.workspace = true
libc.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["fs", "process", "io-util"] }
tokio-util = { workspace = true, features = ["rt"] }
uuid.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

**A6. `rust/crates/facade/Cargo.toml`**: same header treatment. Replace the dependency tables with:

```toml
[dependencies]
agentkit-claude = { workspace = true }
agentkit-native = { workspace = true }
serde.workspace = true
thiserror.workspace = true
tokio = { workspace = true, features = ["fs"] }
tokio-util = { workspace = true, features = ["rt"] }

[dev-dependencies]
async-trait.workspace = true
serde_json.workspace = true
tempfile.workspace = true
```

**A7. `rust/crates/sessions/Cargo.toml`**: add `agentkit-native = { workspace = true }` to `[dependencies]`.

Rule for all manifests: if the compiler asks for a crate that is missing, add it as `<name>.workspace = true`
(it is already pinned in the workspace). If machete reports a line as unused, remove it. Never add a
version number or a path to a crate manifest.

---

## Reference B: the frozen-stanza contract moves to `agentkit-native`

Why: `agentkit-sessions` validates native admission. It may not depend on the executor crate. On
master the validation sat behind `#[cfg(feature = "http")]`, a feature that `agentkit-sessions` does
not have, so without this step the typed validation silently disappears.

**B1. Create `rust/crates/native/src/frozen.rs`.** Move these items out of
`rust/crates/native-exec/src/lib.rs` into it, text unchanged, with their doc comments:

- the four constants `EXECUTION_TARGET_PAYLOAD_KEY`, `TURN_INPUT_PAYLOAD_KEY`, `BROKER_PAYLOAD_KEY`,
  `CONTINUATION_SCOPE_PAYLOAD_KEY`
- `pub struct ContinuationScope` and its whole `impl` block. Change `fn agrees_with` to `pub fn agrees_with`.
- `pub struct BrokerToolSpec`, `pub struct BrokerSpec`, `pub struct BrokerLimitsSpec`
- the bodies of three associated functions of `NativeAgentExecutor`, turned into free functions:

```rust
pub fn parse_target(payload: &Value) -> Result<ExecutionTarget, String> { /* body moved */ }
pub fn validate_frozen_semantics(stanza: &serde_json::Map<String, Value>) -> Result<(), String> { /* body moved */ }
pub fn validate_frozen_stanza(stanza: &serde_json::Map<String, Value>) -> Result<(), String> { /* body moved */ }
```

Inside the moved bodies replace `Self::parse_target` with `parse_target` and
`Self::validate_frozen_semantics` with `validate_frozen_semantics`. Change nothing else.

File header for `frozen.rs`:

```rust
//! The frozen native admission stanza: payload keys, the continuation scope,
//! the broker stanza and the one validator that admission and execution share.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ExecutionTarget, NativeExecutionTarget, TurnInput};
```

**B2. `rust/crates/native/src/lib.rs`**: add `mod frozen;` and

```rust
pub use frozen::{
    BROKER_PAYLOAD_KEY, BrokerLimitsSpec, BrokerSpec, BrokerToolSpec,
    CONTINUATION_SCOPE_PAYLOAD_KEY, ContinuationScope, EXECUTION_TARGET_PAYLOAD_KEY,
    TURN_INPUT_PAYLOAD_KEY, parse_target, validate_frozen_semantics, validate_frozen_stanza,
};
```

**B3. `rust/crates/native-exec/src/lib.rs`**: where the moved items were, add

```rust
pub use agentkit_native::{
    BROKER_PAYLOAD_KEY, BrokerLimitsSpec, BrokerSpec, BrokerToolSpec,
    CONTINUATION_SCOPE_PAYLOAD_KEY, ContinuationScope, EXECUTION_TARGET_PAYLOAD_KEY,
    TURN_INPUT_PAYLOAD_KEY,
};
```

and keep the three associated functions on `NativeAgentExecutor` as one-line delegates, so every
existing caller and test keeps working:

```rust
    pub fn parse_target(payload: &Value) -> Result<ExecutionTarget, String> {
        agentkit_native::parse_target(payload)
    }
    pub fn validate_frozen_semantics(stanza: &serde_json::Map<String, Value>) -> Result<(), String> {
        agentkit_native::validate_frozen_semantics(stanza)
    }
    pub fn validate_frozen_stanza(stanza: &serde_json::Map<String, Value>) -> Result<(), String> {
        agentkit_native::validate_frozen_stanza(stanza)
    }
```

**B4. `rust/crates/sessions/src/session/descriptor.rs`**, in `read_native_execution`: replace

```rust
    #[cfg(feature = "http")]
    if crate::native_executor::NativeAgentExecutor::validate_frozen_stanza(record).is_err() {
```

with

```rust
    if agentkit_native::validate_frozen_stanza(record).is_err() {
```

**B5. `rust/crates/sessions/src/session/mod.rs`**, in `check_frozen`:

- Remove the attribute line `#[cfg(feature = "http")]` above the block that builds `merged`. Keep the block.
- Inside it replace `crate::native_executor::NativeAgentExecutor::validate_frozen_stanza(&merged)`
  with `agentkit_native::validate_frozen_stanza(&merged)`.
- Delete the whole statement that starts with `#[cfg(not(feature = "http"))]` and ends with the
  closing `}` of its `if let Some(stanza) = frozen { ... }`. The typed validator already rejects
  unknown keys.
- Update the doc comments of `check_frozen` and `read_native_execution`: delete the sentences that
  say "with the native lane compiled in" or describe the fallback.

Check: `rg -n 'feature = "http"|native_executor' rust/crates/sessions` prints nothing.

---

## Reference C: compile errors and their fixes

Top of `rust/crates/native-exec/src/lib.rs`: replace the line

```rust
use crate::{ExecutionContext, ExecutionFailure, HostError, HostResult, RetrySafety, TaskExecutor};
```

with

```rust
use agentkit_engine::{ExecutionFailure, RetrySafety, TaskExecutor};

// Transitional bindings to the SQLite store (ADR 0021 amendment). Phase N1
// replaces them with a repository port and a generic executor.
type HostError = agentkit_engine::EngineError<agentkit_store::StoreError>;
type HostResult<T> = Result<T, HostError>;
type ExecutionContext = agentkit_engine::ExecutionContext<SqliteStore>;
```

and replace `use crate::native_bridge::{...}` with `use agentkit_native_broker::{...}`.

| Compiler says | Fix |
|---|---|
| `E0063` missing field `frozen_payload` in `SendInput`, `FollowUpInput` or `SteerInput` | add `frozen_payload: None,` to that literal |
| unresolved `agentkit_host::...` | `AgentSessionService`, `AgentSessionServiceDeps`, `CreateSessionInput`, `SendInput`, `FollowUpInput`, `SteerInput`, `WhenBusy`, `AgentSendResult` come from `agentkit_sessions`. `TaskExecutor`, `Runtime`, `SharedRuntime`, `SystemRuntime`, `ExecutionFailure`, `RetrySafety`, `ClaimInput`, `SubmitTask`, `TaskRecord`, `LeaseFence` come from `agentkit_engine`. `native_executor::X` is `agentkit_native_exec::X`. `native_bridge::X` is `agentkit_native_broker::X`. |
| `AgentHost`, `ExecutionContext`, `HostOptions` inside `native-exec` (it does not depend on sessions) | use the engine generics with the store: `agentkit_engine::AgentHost<SqliteStore>`, `agentkit_engine::ExecutionContext<SqliteStore>`, `agentkit_engine::HostOptions<SqliteStore>` |
| `crate::native_bridge::X` | `agentkit_native_broker::X` |
| `crate::{SharedRuntime, SystemRuntime}` in the executor tests | `agentkit_engine::{SharedRuntime, SystemRuntime}` |
| `agentkit_store::{ClaimInput, SubmitTask, ...}` not found | import those names from `agentkit_engine` |
| `impl TaskExecutor for X` missing generic | `impl TaskExecutor<SqliteStore> for X` |
| `dyn TaskExecutor` missing generic | `dyn TaskExecutor<SqliteStore>` |
| `?` cannot convert `StoreError` into `EngineError<StoreError>` | write `.map_err(HostError::Store)?` on that store call |
| `HostResult` in the example or tests | `agentkit_engine::EngineResult<T, agentkit_store::StoreError>` |
| no method `handles_verification_for` or `lease_fence` | Reference E was skipped |
| `SessionResult`, `SessionError` | `SessionsResult`, `SessionsError` |
| test reads `error.code` | `error.failure_event_code()` (it returns `String`; compare with `== "invalid_input"`) |
| unexpected `cfg` condition value `http` | Reference B was skipped |
| a `rusqlite` row API error in `native_exec.rs` or `native_continuations.rs` | this branch uses rusqlite 0.41. See how commit `349816c` changed `rust/crates/store/src/ts_import.rs` and apply the same pattern |
| a lint from `[lints] workspace = true` in `native`, `claude` or `facade` | smallest mechanical fix, no `#[allow]` |
| `Map` not found in `sessions/src/session/mod.rs` | add `Map` to the existing `use serde_json::{...}` line |

`rust/crates/native-exec/examples/native-chat-lifecycle.rs`: it had `required-features = ["http"]`
only because the module was feature-gated. It needs no feature now. Fix its imports with the table.

`rust/crates/native-broker/tests/live_claude_mcp.rs`: in its doc comment and in the three
`#[ignore = "..."]` reason strings, replace `-p agentkit-host --features http` with
`-p agentkit-native-broker`. Do not add or remove any `#[ignore]`.

---

## Reference D: architecture gate inputs

**D1. `rust/scripts/check-arch.mjs`**, object `LAYERS`, add:

```js
  "agentkit-native": 2,
  "agentkit-native-exec": 3,
  "agentkit-native-broker": 4,
  "agentkit-claude": 4,
  "agentkit-facade": 6,
```

Object `CONTEXT_DEPS`, add:

```js
  "agentkit-native-exec": ["agentkit-engine", "agentkit-sessions"],
```

**D2. `rust/scripts/arch-allowlist.json`**: add exactly these four strings to `violations`, by hand.
Insert each at its alphabetical position (the two `R1b` lines after `R1b agentkit-chat -> agentkit-store`,
the two `R5 agentkit-claude` lines after the last `R5 agentkit-chat` line). Do not touch any other line.

```json
    "R1b agentkit-native-exec -> agentkit-native-broker",
    "R1b agentkit-native-exec -> agentkit-store",
    "R5 agentkit-claude src/backend.rs static GOOD_PATH",
    "R5 agentkit-claude src/discovery.rs static RECORDED_QUALIFICATIONS",
```

The same scan on a trial merge found exactly these two `R5` entries and no stale ones.

**D3. `rust/deny.toml`**: in the `axum` entry of `[bans] deny`, add `"agentkit-native-broker"` to `wrappers`.

---

## Reference E: engine changes that master made in the deleted host

**E1. `rust/crates/engine/src/ports.rs`**, inside `pub trait TaskExecutor<R: TaskRepository>`, after
`resolves_provider_snapshots`:

```rust
    /// Target-aware view of [`Self::handles_verification`]. Wrappers that
    /// route by the frozen execution target MUST override this (and the
    /// sibling below) so the wrapped routes keep the capabilities they
    /// declared: a dispatch wrapper must never silently reclassify a
    /// turn's verification or provider-snapshot behaviour.
    /// Unwrapped executors inherit their flag unchanged.
    fn handles_verification_for(&self, _payload: &serde_json::Value) -> bool {
        self.handles_verification()
    }
    /// Target-aware view of [`Self::resolves_provider_snapshots`].
    fn resolves_provider_snapshots_for(&self, _payload: &serde_json::Value) -> bool {
        self.resolves_provider_snapshots()
    }
```

**E2. `rust/crates/engine/src/context.rs`**, inside `impl<R: TaskRepository> ExecutionContext<R>`:

```rust
    /// The attempt's live lease proof. Lane-side executors use it with the
    /// store's fenced task APIs to settle durable rows under the SAME fence
    /// the runner's claim published; a replaced attempt is refused like
    /// everywhere else.
    #[must_use]
    pub fn lease_fence(&self) -> LeaseFence {
        self.fence.clone()
    }
```

---

## Reference F: documentation edits

**F1. `docs/adr/0025-claude-code-native-backend-contracts.md`**: change the title from `# ADR 0021` to
`# ADR 0025`. Append:

```markdown
## Addendum (2026-11-11): crate placement after ADR 0021

`agentkit-host` no longer exists. The decisions above are unchanged. Where they name
`agentkit-host`, read the table. This ADR was numbered 0021 on `master`; it was renumbered when the
layered-crate ADR 0021 and this work were merged.

| Responsibility | Crate | Layer |
|---|---|---|
| Contracts, capability policy, ports, frozen admission stanza | `agentkit-native` | 2 |
| Generic `NativeAgentExecutor`, `NativeDispatchExecutor` | `agentkit-native-exec` | 3 |
| Task-bound governed MCP broker | `agentkit-native-broker` | 4 |
| Claude Code driver | `agentkit-claude` | 4 |
| Admission of frozen targets | `agentkit-sessions` | 3 |
| Session bindings, interactions, continuation ledger (schema 17 to 19) | `agentkit-store` | 4 |
| Standalone one-turn facade and driver conformance | `agentkit-facade`, to be folded into `agentkit` and `agentkit-testkit` | 6 |

"The host never depends on any driver crate" now reads: no crate in layers 1 to 3 depends on
`agentkit-claude`. `agentkit-native-exec` names `agentkit-native-broker` and `agentkit-store` for
now; both edges are allowlisted and removed by phase N1. Session admission validates the frozen
stanza against the typed contract in every build. File paths in `docs/qualification/` predate this
move and are kept as recorded evidence.
```

In `docs/qualification/AGENTKIT-118-evidence.md` replace `docs/adr/0021-*.md` with
`docs/adr/0025-claude-code-native-backend-contracts.md`. Change nothing else in `docs/qualification/`.

**F2. `docs/adr/0021-layered-crate-architecture.md`**: append:

```markdown
## Amendment (2026-11-11)

- Layer 2 gains `agentkit-native`: ports and contracts for vendor agent runtimes (ADR 0025).
- Layer 3 gains `agentkit-native-exec`. Like the future Hybrid contexts it may depend on engine and
  sessions only.
- Layer 4 gains `agentkit-native-broker` and `agentkit-claude`. `agentkit-store` keeps its name;
  `agentkit-store-sqlite` and `agentkit-store-legacy-import` will not be created (ADR 0024).
- Layer 6 temporarily holds `agentkit-facade`.
- The allowlist only shrinks, with one exception: a merge from `master` may add the entries its
  commit message names. The merge of 2026-11 added four; phases N1 and G1 remove them.
```

**F3. `docs/architecture.md`**: find where the crates are listed per layer
(`rg -n 'agentkit-engine|agentkit-governance' docs/architecture.md`). Add the five crates in the same
format, one line each, with the descriptions from the table in F1.

**F4. `CHANGELOG.md`**: under `## 0.8.0 — unreleased`, after the paragraph that starts with
`**Changed (Rust architecture cleanup).**` and its list, add:

```markdown
**Merged: Claude Code native backend (AGENTKIT-107 to 118) onto the layered crates.** Behaviour is unchanged from `master`; crate paths moved ([ADR 0025](docs/adr/0025-claude-code-native-backend-contracts.md)).

- `agentkit_host::native_executor::*` -> `agentkit_native_exec::*` (`NativeAgentExecutor`, `NativeDispatchExecutor`, `AttemptBrokerFactory`, `NativeExecutorOptions`).
- `agentkit_host::native_bridge::*` -> `agentkit_native_broker::*` (`AttemptBroker`, `BrokerAuthority`, `BrokerBackend`, `BrokerLimits`).
- The frozen admission contract (the four payload keys, `ContinuationScope`, `BrokerSpec`, `validate_frozen_stanza`) lives in `agentkit-native`; `agentkit-native-exec` re-exports the types.
- Session admission validates the frozen stanza against the typed contract in every build (no `http` feature gate).
- `TaskExecutor` gains `handles_verification_for` and `resolves_provider_snapshots_for`; `ExecutionContext` gains `lease_fence()`. Store schema version is 19.
```

**F5. Doc comments** in `rust/crates/native/src`, `rust/crates/claude/src/lib.rs`,
`rust/crates/facade/src/lib.rs`, `rust/crates/native-exec`, `rust/crates/native-broker`: where a
comment says `agentkit-host`, write `agentkit-native-exec` (for the executor), `agentkit-engine` (for
tasks, attempts, leases) or `agentkit-store`, whichever the sentence means. Comments only.
