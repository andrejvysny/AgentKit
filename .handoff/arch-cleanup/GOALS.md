# AgentKit handoff: two goals for omp goal mode (Qwen3.8 Flash Next)

Prepared 2026-11-11. Repository: `~/workspace/onemind-workspace/AgentKit`.

## What is in this folder

| File | Purpose |
|---|---|
| `GOALS.md` | This file. The two goal texts to paste, and how to run them. |
| `GOAL-1-MERGE.md` | Runbook for goal 1: merge `master` into `arch/cleanup`. |
| `GOAL-2-CLEANUP.md` | Runbook for goal 2: gates, native-lane ports, facade fold, open builder. |
| `agentkit-target-architecture.html` | The target-architecture page as a standalone file. Reference only. |

The folder is `.handoff/arch-cleanup/` inside the repository. It is untracked. The first command of
each runbook adds `/.handoff/` to `.git/info/exclude` (a local ignore that is never committed), so
the files stay out of `git status` and survive branch switches. Until then `git status` shows
`?? .handoff/`. If you move the folder, replace `.handoff/arch-cleanup` in the two goal texts and
in the runbooks.

## How to run a goal

1. `cd ~/workspace/onemind-workspace/AgentKit`. `git status` should show nothing except `?? .handoff/`.
2. Start `omp` with Qwen3.8 Flash Next. Keep thinking on and leave reasoning effort at its default.
   The model card says lower effort does not shorten agentic tasks, it adds retries.
3. Type `/goal`, paste the goal text from below into the editor, and confirm.
4. Optional: `/goal budget <tokens>`. Budgets count input, output and cache-write tokens. Estimates,
   not measurements: goal 1 around 10 to 20 million, goal 2 around 20 to 40 million. A budget stop is
   not completion: the agent writes its state to the progress file, and `/goal resume` continues.
5. Run the goals one after the other. Goal 2 starts only after goal 1 reports complete.

Between the goals, as the owner:

- Read `.handoff/arch-cleanup/REPORT-1.md`.
- Re-run the four ignored live tests against the real Claude CLI (they spend subscription quota):
  `AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-native-broker --test live_claude_mcp -- --ignored --nocapture`
  and `AGENTKIT_LIVE_CLAUDE=1 cargo test -p agentkit-facade --test live_claude -- --ignored --nocapture`.
- Push `arch/cleanup`, mark PR #4 ready and merge it. Goal 2 then branches from `origin/master`.
  If you start goal 2 before PR #4 lands, it branches from `arch/cleanup` instead.

---

## Goal 1 text (paste into `/goal`)

```
Merge `master` into `arch/cleanup` in this repository (AgentKit) by executing the runbook `.handoff/arch-cleanup/GOAL-1-MERGE.md` stage by stage, S0 to S10, in order, without skipping. Read the whole runbook before the first command. After every context compaction re-read its sections 1 and 2, the stage you are in, and `.handoff/arch-cleanup/PROGRESS-1.md`. Record every stage result in that progress file as you go.

The goal is complete only when all of these are true and you have verified each one with the commands in runbook section 2:
1. `arch/cleanup` points to one merge commit with exactly two parents, `4a6342f` and the `master` tip, and its tree equals the branch `integration/master-merge`.
2. All 16 gates pass on that tree, with logs in `.handoff/arch-cleanup/logs/`.
3. No test was deleted, weakened or newly ignored: at least 2113 test attributes under `rust/` and exactly 12 `#[ignore]`.
4. `rust/scripts/arch-allowlist.json` differs from commit `4a6342f` by exactly the four added entries the runbook lists.
5. `rust/crates/sessions` contains no `cfg(feature = "http")`, there are no conflict markers, and `rust/crates/host` does not exist.
6. `.handoff/arch-cleanup/REPORT-1.md` exists and follows the runbook template.

Forbidden: pushing; touching PR #4; committing on `master`; moving `arch/cleanup` before stage S10; `check-arch --update-allowlist`; adding `#[ignore]` or `#[allow]`; editing test assertions; changing the behaviour of the native executor, the broker or session admission; installing tools; deleting the integration branch; printing full build or test logs.

If the same check fails twice for the same reason, or the runbook does not cover a situation, stop, write what you found under BLOCKED in the progress file, and report. Do not improvise. A final answer that lists remaining work means the goal is not complete.
```

---

## Goal 2 text (paste into `/goal` after goal 1 is complete)

```
Continue the AgentKit architecture cleanup by executing the runbook `.handoff/arch-cleanup/GOAL-2-CLEANUP.md`, work packages WP0 to WP6, in order, without skipping. Read the whole runbook and the three ADRs it names before the first command. After every context compaction re-read its sections 1 and 2, the work package you are in, and `.handoff/arch-cleanup/PROGRESS-2.md`. Record every work-package result in that progress file as you go.

The goal is complete only when all of these are true and you have verified each one with the commands in runbook section 2:
1. Four stacked branches exist: `arch/g1-gates`, `arch/n1-native-ports`, `arch/n1-fold-facade`, `arch/c1-open-builder`, each containing the previous one, and all 16 gates pass on the last one.
2. `check-arch` detects real shared mutable state, has the new rules R8, R11 and R12, and prints one count per rule.
3. `agentkit-native-exec` has no normal dependency on `agentkit-store`, `agentkit-native-broker`, `agentkit-mcp`, axum, rmcp or rusqlite. The ports `AttemptBrokerHandle` and `NativeRunRepository` live in `agentkit-native`. The executor's 42 inline tests still exist and pass.
4. `rust/crates/facade` is gone. `NativeChat` lives in the `agentkit` crate behind the feature `claude`, and the driver conformance harness lives in `agentkit-testkit`.
5. `AgentRuntimeBuilder` has `turn_kinds`, `executor` and `native_lane`, covered by `rust/crates/runtime/tests/native_lane.rs`, and ADR 0026 exists.
6. No test was deleted, weakened or newly ignored compared with the WP0 baseline.
7. `.handoff/arch-cleanup/REPORT-2.md` exists and follows the runbook template.

Forbidden: pushing; opening or merging pull requests; committing on `master` or `arch/cleanup`; changing behaviour; adding `#[ignore]` or new `#[allow]`; editing test assertions; running `check-arch --update-allowlist` more than the one time WP1 allows; changing dependency versions; starting anything listed in runbook section 6; printing full build or test logs.

Each work package must end with all 16 gates green and a commit before the next one starts. If the same check fails twice for the same reason, or the runbook does not cover a situation, stop, write what you found under BLOCKED in the progress file, and report. Do not improvise. A final answer that lists remaining work means the goal is not complete.
```

---

## Why the goals are written this way

- omp stores the goal text with the session and keeps working until the agent verifies completion,
  so each goal names its deliverables, the evidence of completion and the forbidden shortcuts. The
  long instructions live in the runbooks, which the agent can re-read after compaction.
- A Flash-tier model follows explicit steps well and drifts when it has to choose. The runbooks give
  exact commands, exact file contents where they could be decided in advance, a table that maps each
  compiler error class to its fix, and a stop rule instead of open-ended judgement.
- Goal 1 never touches `arch/cleanup` until the very end. All iteration happens on
  `integration/master-merge`, and the final step creates one merge commit from a tree that already
  passed every gate. If the agent stops halfway, the branch behind PR #4 is unchanged.
- Nothing in either runbook was compiled when it was written. The conflict list, the six file moves,
  the commit-tree step, the two new `R5` findings and the test-attribute counts were checked with a
  trial merge in a scratch clone. Manifests and compile fixes are derived from reading the code, so
  the compile loop in stage S3 is expected to need several rounds.
