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
| S2 | Read/search exploration with native tools, no shell | `packages/coding-agent/test/tools.test.ts`, `read-line-range-display.test.ts`, `tool-search.test.ts`; OneMind `pi-worker/search.test.ts` | M4 coding tool pack | pinned (source) |
| S3 | Edit file with conflict detection | `packages/coding-agent/test/edit-tool-legacy-input.test.ts`, `file-mutation-queue.test.ts` | M4 | pinned (source) |
| S4 | Run tests / bounded shell with timeout and teardown | `packages/coding-agent/test/exec.test.ts`, `bash-close-hang-windows.test.ts`; OneMind `pi-worker/shell-isolation.test.ts`, `bash-exploration.test.ts` | M4/M5 | pinned (source) |
| S5 | Multi-tool turn, tool results balanced, parallel vs sequential | `packages/agent/test/agent-loop.test.ts` (`executeToolCalls*`), `packages/coding-agent/test/nested-tool-calls.test.ts` | M1 (loop), exists (`golden: tool-run`, `tool-cap-run`) | AgentKit (M1) |
| S6 | Tool gated by host approval; denial fed back to model | OneMind `pi-worker/gate.test.ts` | M1 deny-only hook + existing `ToolGuard`; M5 durable approvals | AgentKit (M1/M5) |
| S7 | Follow-up turn after settlement | `packages/agent/test/agent.test.ts` (`followUp`) | M2 | AgentKit (M2); not a OneMind blocker |
| S8 | Mid-run steering placed after the current tool batch | `packages/agent/test/agent.test.ts` (steering, `continue()` semantics) | M2 | AgentKit (M2); not a OneMind blocker |
| S9 | Abort mid-stream / mid-tool; no post-cancel dispatch | `packages/agent/test/agent.test.ts`, `agent-loop.test.ts`; OneMind `pi-worker/worker.test.ts` | exists (`golden: cancelled-run`, `run-loop-cancellation*.test.ts`) | AgentKit (done) |
| S10 | Resume exact session after quit/restart | `packages/coding-agent/test/sdk-session-manager.test.ts`, `session-file-invalid.test.ts`; OneMind `pi-worker/session.test.ts` | M2 | AgentKit (M2) |
| S11 | Fork / branch session from a checkpoint | `packages/coding-agent/test/agent-session-branching.test.ts`, `agent-session-tree-navigation.test.ts` | exists (chat fork, ADR 0007) + M2 session fork | AgentKit (M2) |
| S12 | Auto compaction at threshold; overflow recovery | `packages/coding-agent/test/agent-session-compaction.test.ts`, `agent-session-auto-compaction-queue.test.ts`, `compaction*.test.ts`; OneMind `pi-worker/worker.test.ts` (`worker compaction`) | M3 | pinned (source) |
| S13 | Model change mid-session with continuation safety | `packages/coding-agent/test/agent-session-dynamic-provider.test.ts` | M2 (`setModel`) + existing continuation validation | AgentKit (M2) |
| S14 | Retry on transient provider failure | `packages/coding-agent/test/agent-session-retry.test.ts` | exists (host retry / pass boundary) | AgentKit (done) |
| S15 | Event stream fidelity for a UI (start / deltas / tool start-end / settled) | OneMind `pi-worker/events.test.ts`, `pi/parser.test.ts` | M1 event mapping (ADR 0015 §event mapping) | AgentKit (M1) |
| S16 | End-to-end coding task: read → edit → test → review → follow-up → compact → stop → resume | composite of S2–S12 | M7 example + M8 side-by-side | BLOCKED (M6) |

## Crash matrix (canonical page §12)

Every case requires explicit durable state, no lost authoritative event, and no unauthorized duplicate effect.

| # | Crash point | AgentKit owner | Status |
|---|---|---|---|
| C1 | Before model request | M2 crash matrix | AgentKit (M2) |
| C2 | Mid-stream | exists: `packages/runner-local/tests/e2e-vertical-slice.test.ts` slice D (the provider parks past the lease TTL, and a second runner recovers onto attempt 2) | AgentKit (done) |
| C3 | After model reply, before durable log | M2 crash matrix | AgentKit (M2) |
| C4 | Before tool call | M2 crash matrix | AgentKit (M2) |
| C5 | After tool side effect, before receipt | M2 (records current at-least-once behaviour); M5 receipts | AgentKit (M2/M5) |
| C6 | While waiting for human input | M2 | AgentKit (M2) |
| C7 | After compaction, before checkpoint | M3 | pinned (source) |
| C8 | Application quit and OS kill | M2 (restart), M8 (packaged app) | AgentKit (M2/M8) |

## Security matrix (canonical page §12)

| # | Case | AgentKit owner | Status |
|---|---|---|---|
| X1 | Parent-relative and absolute path escapes | M4 | pinned (OneMind `pi-worker/tools.ts` `confinePath`) |
| X2 | Symlinks and races (TOCTOU) | M4/M5 | pinned (source) |
| X3 | Credential and hidden files excluded from reads | M4 | pinned (source) |
| X4 | Git hooks and untrusted repository instructions | M3/M5 | open |
| X5 | Shell injection, network and exfiltration | M5 (OS sandbox) | pinned (OneMind `pi-worker/shell-isolation.test.ts`) |
| X6 | MCP tools misclassified as reads | M5 (reuse Code Mode A10) | open |
| X7 | Tool cancellation and stale callbacks | M1 (no post-cancel dispatch) | AgentKit (M1) |
| X8 | Permission mode / role inversion | M5/M7 | open |
| X9 | A loop hook cannot widen authority | M1 (deny-only gate, ToolGuard stays final) | AgentKit (M1) |
