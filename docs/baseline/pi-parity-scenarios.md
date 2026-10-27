# Pi / OneMind parity scenario manifest (M0)

This is the behavior corpus the unified AgentKit runtime must match before OneMind can migrate off its bundled Pi
worker (canonical roadmap M8, ADR [0015](../adr/0015-unified-agent-runtime.md)). It lists scenarios and where each one
is already exercised. **It contains no recorded traces yet**:
- A trace that compares behaviour needs the same task, run on the same model, against both runtimes.
- That needs a live tool-capable model, which is M6 work.
- Until then, every row is pinned by the source tests it names, and by AgentKit tests once the owning milestone lands.

Reference versions (see [m0-2026-10-27.md](m0-2026-10-27.md)):
- Pi behaviour is the **vendored v1.4.0** (`4a08025`) that OneMind ships.
- Pi test paths below are relative to the Pi repository root, read from the local checkout (`db14038`, v1.4.0 + 88).
- OneMind paths are relative to `OneMind/src-ts/src/code/`.

OneMind uses one prompt per turn. It never calls Pi's `steer`/`followUp`; `grep` finds neither under `pi*/`. Steering
and follow-up are therefore AgentKit product features, not OneMind migration blockers. Their rows are marked
accordingly.

Status values:
- **pinned (source)**: the named Pi/OneMind tests cover it. There is no AgentKit equivalent yet.
- **AgentKit (milestone)**: AgentKit coverage lands in that milestone.
- **BLOCKED (M6)**: needs live-model recording.

## Agent behaviour scenarios

| # | Scenario | Pi / OneMind source | AgentKit owner | Status |
|---|---|---|---|---|
| S1 | Single prompt, model answers with no tools | `packages/agent/test/agent-loop.test.ts`; OneMind `pi-worker/worker.test.ts` | exists (`golden: chat-only`) | AgentKit (done) |
| S2 | Read/search exploration with native tools, no shell | `packages/coding-agent/test/tools.test.ts`, `read-line-range-display.test.ts`, `tool-search.test.ts`; OneMind `pi-worker/search.test.ts` | `packages/toolpacks-coding` (`read_file` ranges/batches/sha256, `code_search` paging, gitignore-aware `find_files`/`list_dir`); `tests/read-file.test.ts`, `tests/search.test.ts` | AgentKit (done) |
| S3 | Edit file with conflict detection | `packages/coding-agent/test/edit-tool-legacy-input.test.ts`, `file-mutation-queue.test.ts` | `edit-engine.ts` (expected sha256, overlap/ambiguity refusal, per-line EOL kept, invalid UTF-8 refused); `tests/edit-file.test.ts` | AgentKit (done) |
| S4 | Run tests / bounded shell with timeout and teardown | `packages/coding-agent/test/exec.test.ts`, `bash-close-hang-windows.test.ts`; OneMind `pi-worker/shell-isolation.test.ts`, `bash-exploration.test.ts` | `exec-tools.ts`, `process.ts`, `process-tree.ts` (TERM→KILL, escaped-descendant tracking, orphan sweep); `tests/exec.test.ts`, `tests/teardown.test.ts` | AgentKit (done, macOS + Linux) |
| S5 | Multi-tool turn, tool results balanced, parallel vs sequential | `packages/agent/test/agent-loop.test.ts` (`executeToolCalls*`), `packages/coding-agent/test/nested-tool-calls.test.ts` | M1 (loop), exists (`golden: tool-run`, `tool-cap-run`) | AgentKit (M1) |
| S6 | Tool gated by host approval; denial fed back to model | OneMind `pi-worker/gate.test.ts` | `packages/host/src/sessions/human-answer.ts`, `packages/host/src/turn/approval-resume.ts`; `packages/host/tests/approval-resume.test.ts`, `examples/coding-agent/tests/acceptance.test.ts` | AgentKit (done) |
| S7 | Follow-up turn after settlement | `packages/agent/test/agent.test.ts` (`followUp`) | M2 | AgentKit (M2); not a OneMind blocker |
| S8 | Mid-run steering placed after the current tool batch | `packages/agent/test/agent.test.ts` (steering, `continue()` semantics) | M2 | AgentKit (M2); not a OneMind blocker |
| S9 | Abort mid-stream / mid-tool; no post-cancel dispatch | `packages/agent/test/agent.test.ts`, `agent-loop.test.ts`; OneMind `pi-worker/worker.test.ts` | exists (`golden: cancelled-run`, `run-loop-cancellation*.test.ts`) | AgentKit (done) |
| S10 | Resume exact session after quit/restart | `packages/coding-agent/test/sdk-session-manager.test.ts`, `session-file-invalid.test.ts`; OneMind `pi-worker/session.test.ts` | M2 | AgentKit (M2) |
| S11 | Fork / branch session from a checkpoint | `packages/coding-agent/test/agent-session-branching.test.ts`, `agent-session-tree-navigation.test.ts` | exists (chat fork, ADR 0007) + M2 session fork | AgentKit (M2) |
| S12 | Auto compaction at threshold; overflow recovery | `packages/coding-agent/test/agent-session-compaction.test.ts`, `agent-session-auto-compaction-queue.test.ts`, `compaction*.test.ts`; OneMind `pi-worker/worker.test.ts` (`worker compaction`) | `packages/host/src/context/` (compaction, stale tool-result elision); `packages/host/tests/context-compaction.test.ts`; live: `scripts/live/compaction-session.ts` | AgentKit (done) |
| S13 | Model change mid-session with continuation safety | `packages/coding-agent/test/agent-session-dynamic-provider.test.ts` | M2 (`setModel`) + existing continuation validation | AgentKit (M2) |
| S14 | Retry on transient provider failure | `packages/coding-agent/test/agent-session-retry.test.ts` | exists (host retry / pass boundary) | AgentKit (done) |
| S15 | Event stream fidelity for a UI (start / deltas / tool start-end / settled) | OneMind `pi-worker/events.test.ts`, `pi/parser.test.ts` | M1 event mapping (ADR 0015 §event mapping) | AgentKit (M1) |
| S16 | End-to-end coding task: read → edit → test → review → follow-up → compact → stop → resume | composite of S2–S12 | `examples/coding-agent/tests/acceptance.test.ts` (real tools, real sqlite, scripted provider); the live-model half is M8 side-by-side | AgentKit (done, offline) |
| S17 | Plan mode: reads allowed, writes refused finally until a human approves the plan | no Pi equivalent (Pi has no plan mode) | `packages/toolpacks-coding/src/guard.ts`, `packages/host/src/sessions/plan-mode.ts`; `packages/toolpacks-coding/tests/plan-flow.test.ts`, `packages/host/tests/plan-mode.test.ts` | AgentKit (done) |
| S18 | Profile tool allowlist and role ceiling enforced by the host, not by prompt text | Pi: role is prompt text only | `packages/host/src/profiles/agent-profile.ts`, `packages/host/src/turn/registry-staging.ts`; `packages/host/tests/profiles.test.ts` | AgentKit (done) |

## Crash matrix (canonical page §12)

Every case requires explicit durable state, no lost authoritative event, and no unauthorized duplicate effect.

| # | Crash point | AgentKit owner | Status |
|---|---|---|---|
| C1 | Before model request | M2 crash matrix | AgentKit (M2) |
| C2 | Mid-stream | exists: `packages/runner-local/tests/e2e-vertical-slice.test.ts` slice D (the provider parks past the lease TTL, and a second runner recovers onto attempt 2) | AgentKit (done) |
| C3 | After model reply, before durable log | M2 crash matrix | AgentKit (M2) |
| C4 | Before tool call | M2 crash matrix | AgentKit (M2) |
| C5 | After tool side effect, before receipt | `packages/host/src/turn/side-effects.ts`, `packages/host/src/turn/history-reconcile.ts`; `packages/host/tests/side-effects.test.ts`, `packages/runner-local/tests/e2e-sessions-crash.test.ts` (C5/C5b), `examples/coding-agent/tests/acceptance.test.ts` (SIGKILL of a real child process) | AgentKit (done) |
| C6 | While waiting for human input | M2 | AgentKit (M2) |
| C7 | After compaction, before checkpoint | M3 | pinned (source) |
| C8 | Application quit and OS kill | `shutdownMode: "interrupt"` + `recoveryMode: "manual"`; `packages/runner-local/tests/interrupt.test.ts`, `examples/coding-agent/tests/acceptance.test.ts` (quit, SIGKILL); packaged app is M8 | AgentKit (done); M8 open |

## Security matrix (canonical page §12)

| # | Case | AgentKit owner | Status |
|---|---|---|---|
| X1 | Parent-relative and absolute path escapes | `packages/toolpacks-coding/src/paths.ts`; `tests/paths.test.ts` | AgentKit (done) |
| X2 | Symlinks and races (TOCTOU) | M4/M5 | pinned (source) |
| X3 | Credential and hidden files excluded from reads | `paths.ts` sensitive-name list, sandbox credential read-deny; `tests/paths.test.ts`, `tests/read-file.test.ts`, `tests/exec.test.ts` | AgentKit (done) |
| X4 | Git hooks and untrusted repository instructions | `git-tool.ts` neutralizes hooks, fsmonitor and repo-defined filter/textconv/ext-diff drivers; sandbox makes `.git` read-only; project instructions are labelled untrusted (`context/instructions.ts`); `tests/git-inspect.test.ts` | AgentKit (done) |
| X5 | Shell injection, network and exfiltration | `sandbox.ts` (network off, writes confined), env scrub in `exec-tools.ts`; `tests/exec.test.ts` (macOS only; other OSes refuse unless `sandbox: "off"`) | AgentKit (done, macOS) |
| X6 | MCP tools misclassified as reads | M5 (reuse Code Mode A10) | open |
| X7 | Tool cancellation and stale callbacks | M1 (no post-cancel dispatch) | AgentKit (M1) |
| X8 | Permission mode / role inversion | `packages/toolpacks-coding/src/guard.ts` (the ceiling is checked before the mode and its refusal carries no approval), `packages/host/src/profiles/agent-profile.ts`; `packages/toolpacks-coding/tests/guard.test.ts`, `packages/host/tests/profiles.test.ts` | AgentKit (done) |
| X9 | A loop hook cannot widen authority | M1 (deny-only gate, ToolGuard stays final) | AgentKit (M1) |
