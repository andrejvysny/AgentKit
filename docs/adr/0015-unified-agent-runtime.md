# ADR 0015 — Unified agent runtime: one loop, one durable host, profiles not engines

**Status:** accepted (2026-10-27). M0, M1 and M2 implemented (Plane AGENTKIT-30..33).
M3 [0017](0017-context-assembly-and-compaction.md), M4 [0018](0018-coding-tool-pack.md) and M5
[0019](0019-human-approval-plan-mode-and-side-effect-receipts.md) are implemented; M7's standalone coding-agent
example exists (`examples/coding-agent`), and M6/M8 (live-model parity traces, OneMind migration) are open.
**Contract impact:** none for M0. M1 is additive: one new `run.warning` code (`hook_failed`), new in-process types
(`runAgentLoop`, `AgentLoopHooks`), and no DTO changes. `CONTRACT_VERSION` stays `0.7.0`, per
[contracts.md](../contracts.md#contract_version-policy).

The product and roadmap decision lives in the Plane page "AgentKit — Canonical Unified Agent Runtime Specification &
Roadmap". This ADR is the code-coupled half: the rules the source must obey, the ownership boundaries, the M1 hook
contract, and how the earlier plans map onto the M0–M9 milestones. The baseline it is measured against is
[docs/baseline/m0-2026-10-27.md](../baseline/m0-2026-10-27.md).

## Problem

AgentKit is an embeddable chat-with-tools runtime: `runChat`, `TurnRunner`, durable tasks, conversations and MCP.
Consumers now want coding, research and automation agents from it. OneMind already ships a coding agent, but it runs on
a bundled Pi worker (Pi v1.4.0). That means a second model/tool loop, a second session store (JSONL) and a second
settings/auth surface.

Two failure modes are easy to drift into:
- **Two engines.** Shipping Pi's loop beside `runChat`, or wrapping one in the other, gives two tool-invocation owners,
  two histories, two approval paths, and recovery that neither side fully owns.
- **A rewrite.** Replacing `runChat`/`TurnRunner` discards the hardening already pinned by tests: cancellation balance,
  lease fencing, first-terminal-wins, and continuation scope (ADR 0006 and ADR 0014).

## Evidence

- `runChat` (`packages/core/src/runs/run-loop.ts`) is the only provider/tool loop in the repository today.
  - `client.streamChat(` is called only at `run-loop.ts:196`. The `bounded-provider.ts` wrapper delegates to it.
  - Every model-requested tool runs through `executeToolSafely` (`run-loop.ts:1089`).
  - Other `.execute(` sites are wrappers or different surfaces: `bounded-run.ts` (budget), `registry-staging.ts` (the
    guard chain), `mcp-server/src/tool-source.ts` (inbound MCP serving), and the task executor/worker dispatch.
- `TurnRunner.runPass` (`packages/host/src/turn/turn-runner.ts:963`) is the only host call into `runChat`.
- Host policy already sits inside the tool: `ToolGuard.canExecute` runs in the staged wrapper (`registry-staging.ts:311`)
  around `tool.execute`. Anything the loop does before calling `execute` is therefore structurally before authorization.
- These are declared but unused or missing:
  - `waiting_approval` is declared (`packages/host/src/ports/task-store.ts:20-27`) and produced by nothing.
  - There is no session, steering, follow-up, profile or compaction code (grep, M0).
- OneMind drives Pi with one prompt per turn and never uses `steer`/`followUp`. Its value-add is the host gate, the
  bootstrap handshake, the session namespace and the compaction bridge (`OneMind/src-ts/src/code/pi-worker/`).

## Decision

### 1. One loop

`@agentkit/core` has exactly one model/tool iteration: `runAgentLoop`, extracted from `runChat` in M1.
- `runChat` remains the public facade with unchanged input, output, defaults and events. It is implemented by calling
  `runAgentLoop`.
- No second loop is added, and none is nested inside `runChat`. Pi's loop is not shipped.
- An architecture test (`packages/testing/tests/architecture.test.ts`, M1) allowlists every `.streamChat(` and
  `.execute(` call site in `packages/*/src`. A new one fails the build until it is reviewed.

### 2. One durable owner

`@agentkit/host` (TaskService, TaskStore, TaskRunner, TurnRunner) owns:
- tasks, attempts, leases and fencing;
- the event log, approvals and receipts;
- usage and recovery;
- the authoritative terminal outcome.

Core never persists anything. No Pi Durable, no second scheduler, no second task store. Specialised workflows (coding,
research, Supervisor → Implementer → Reviewer) are compositions of host tasks, using the existing child-task and
dependency support from ADR 0003. They are not recursive engines.

### 3. Profiles, not engines

Chat, Coding, Research, Automation, Supervisor, Implementer and Reviewer are configurations of the same runtime. A
profile is tools, prompts, selected context, model, budgets and permission ceilings (M7). A profile is resolved and
pinned at admission. It never changes an in-flight attempt.

Wiring, as enforced by the host: `model` sits between the session pin and the provider default; `reasoningEffort`
(session pin, else profile) reaches the provider as `reasoning_effort` / `reasoning.effort` and, when a
chat-completions server rejects it, is dropped once with a `reasoning_effort_unsupported` warning;
`maxToolCallsPerIteration` and `maxOutputTokens` flow into the loop and provider request; `attemptBudgetMs` arms the
execution budget so an overrun lands `failed` (`execution_timeout`); `resources.projectInstructions: false` skips
workspace instruction loading; `resources.skills` are appended to the session's skills (the session's revision wins).
`run.usage` also carries `cachedPromptTokens` and `reasoningTokens` when the provider reports them.

### 4. Identity model

| Id | Meaning | Owner |
|---|---|---|
| session id | A persistent logical agent conversation. In M2 it equals the chat id, and the session descriptor lives on the chat (ADR 0016). | host |
| turn id | One user → model exchange. Equals the task id of its `chat.turn` task (`runId == taskId` today). | host |
| task id / attempt id / lease token | One durable execution and its fenced attempts. | host |
| step | One provider round-trip inside a turn (`run.usage.data.step`, hook `step`). | core |

### 5. Pi is a reference and an MIT source donor

- Study Pi's loop, session, compaction and tool behaviour, and test against it (scenario manifest:
  [pi-parity-scenarios.md](../baseline/pi-parity-scenarios.md)).
- Adapting a small Pi file is allowed. When that happens:
  - the file carries a header naming the upstream path, commit and the MIT notice ("Copyright (c) 2025 Mario
    Zechner");
  - `PROVENANCE.md` gets an entry.
- No `@earendil-works/*` or `@mariozechner/*` runtime dependency, no vendored monorepo, no Pi settings/auth store, and
  no Pi CLI/TUI. The M1 architecture test enforces the no-dependency rule.
- M0–M2 adapt no Pi code.

### 6. M1 loop hooks (Rig TS-01 + a narrow TS-02)

`runAgentLoop` accepts `hooks?: readonly AgentLoopHooks[]` and `hookTimeoutMs?` (default 5000). `RunChatInput` does not
gain these fields, so `runChat` callers cannot pass hooks.

| Hook | When | Effect |
|---|---|---|
| `onRunStart` | once, before the first step | observe |
| `onStepStart` | before each provider request | observe |
| `onModelResponse` | after a step's response is assembled, before any tool runs | observe |
| `beforeToolCall` | after argument parse and schema validation, before `execute` | **deny only** |
| `onToolResult` | after each tool's terminal event | observe |
| `onSettled` | once, after the run's terminal event, on every terminal including budget failures | observe, best-effort |

How hooks run:
- **Ordering.** Hooks run sequentially in array order at each point. Each call is raced against `hookTimeoutMs`, with
  the same race semantics as `withHookDeadline` (a late hook is ignored, not cancelled). Each call receives the run's
  `AbortSignal` so it can cooperate.
- **Observer failure is diagnostic.** A throw emits `run.warning` `hook_failed`. A timeout emits `hook_timeout`. The
  message names the hook and its index but never the thrown text, which could be a stack or a secret. The run
  continues.
  - `onSettled` failures are swallowed, because nothing may follow a terminal event.
- **The gate fails closed.** A `deny`, a throw, or a timeout from any `beforeToolCall` means the tool is not executed:
  - The loop emits `run.tool.failed` with `errorCode: "tool_hook_denied"`, `phase: "guard"` and `retryable: false`.
  - The model receives the usual error envelope.
  - History stays balanced.
- **A hook cannot widen authority.**
  - Its `args` is a frozen clone; the tool receives the loop's own parsed arguments.
  - There is no rewrite, grant or "skip guard" outcome.
  - Host `ToolGuard`s still run inside `execute`, after the hook, so `allow` from a hook can never override a guard's
    refusal.
- **Payloads are audit-safe.** No `continuation`, `continuationScope`, `runContent`, provider credentials or client
  handles appear in any hook context.
- **No hooks, no change.** With `hooks` absent, the loop takes no extra awaits and emits a byte-identical event stream.
  The golden traces are the proof.

Argument and result rewriting, `prepareRequest`-style model or context replacement, and steering inputs are **not** M1.
- Rewriting needs the M5 governance contract (Code Mode A03 invocation broker).
- Context replacement is M3.
- Steering inputs are M2 and need durable receipts.

### 7. Step budgets

The existing knobs are the step budgets, and M1 adds none:
- `maxToolIterations`, default 4 and clamped to ≥ 1, caps provider round-trips per pass.
- `maxToolCallsPerIteration` (default 8) caps calls per step.
- `ExecutionBudgets` (`packages/core/src/runs/execution-budget.ts`) cap wall time, first byte, stream idle, provider
  requests, tool calls, correction passes and retries across passes.

Long coding turns raise these per profile (M7). The default stays 4 for backward compatibility.

### 8. Event mapping (Pi → AgentKit)

| Pi `AgentEvent` / `AgentSessionEvent` | AgentKit |
|---|---|
| `agent_start` | `run.started` (first step only) |
| `turn_start` / `turn_end` | step boundary: `onStepStart` hook, `run.usage.data.step` |
| `message_update` | `run.message.delta`, `run.message.reasoning-delta` |
| `message_end` | `run.message.completed` |
| `tool_execution_start` | `run.tool.requested` + `run.tool.running` |
| `tool_execution_end` | `run.tool.succeeded` / `run.tool.failed` |
| `agent_end` | `run.completed` / `run.failed` / `run.cancelled` |
| `agent_settled` | task terminal status (the stream closes on the task terminal, ADR 0014) |
| `queue_update` | M2 `run.input.applied` (ADR 0016) |
| `compaction_start` / `compaction_end` | M3 |
| `auto_retry_*` | `run.warning` `retry_pass` (host pass boundary) |

## Gap map (AgentKit at `fed11f1` vs Pi v1.4.0)

| Area | AgentKit has | Pi has | Milestone |
|---|---|---|---|
| Loop | `runChat`: cancellation balance, schema validation, caps, budgets, Responses continuation | stateful `Agent`, `prepareRequest`/`prepareNextTurn`/`finishTurn`, parallel tools | M1 (hooks), M7 (parallel tools per profile) |
| Steer / follow-up | none | in-memory queues (one-at-a-time) | M2 (durable) |
| Sessions | durable chat trees, branches, fork, import, SSE resume | JSONL session tree v3 | M2 (session = chat) |
| Human wait | `waiting_approval` declared, unproduced | extension UI / AskUserQuestion via host | M2 |
| Context / compaction | `HistoryLimit` (count), selected skills and prompts | token-aware auto/manual compaction | M3 |
| Tools | Ajv/TypeBox schemas, `effect` metadata, ToolGuard, proposals, MCP | read/grep/find/ls/edit/write/bash | M4 (coding pack), M5 (sandbox, receipts) |
| Providers | OpenAI-compatible, Responses, continuation scope, credentials | many providers, thinking levels | M6 |
| Orchestration | child tasks, dependencies, leases | none (single agent) | M7 |

## Reconciliation with earlier plans

| Earlier item | Unified milestone |
|---|---|
| Rig TS-00 baseline | M0 (this ADR plus the baseline record) |
| Rig TS-01 observer hooks, TS-02 steering hooks / final authorization | M1 (observer + deny-only). Rewriting is deferred to M5. |
| Rig TS-03/04 effect cassettes | M6 (deterministic effect cassettes) |
| Rig TS-05 model metadata, TS-06 ModelRouter | M6 |
| Rig TS-07/08 token budget, compaction | M3 |
| Rig TS-09 typed extraction | M6 (quality). Independent of the loop. |
| Rig TS-10/11 memory / RAG (existing P6) | M3 retrieval, extending P6 rather than forking it |
| Rig TS-12 telemetry / cost | M6 |
| Rig TS-13/14 checkpoint, approval wait | M2 (first `waiting_approval` producer, human input), M5 (durable approvals, receipts) |
| Rig TS-15 rollout | M9 |
| AGENTKIT-12..29 Code Mode | M5 reuses A01–A17 governance (invocation broker, effect classification, receipts). It is not the coding-agent runtime. |
| AGENTKIT-1..11 Desktop / Responses | Prerequisites, kept separate. Responses continuation rules bind M2 `setModel` and fork. |

## Threats (summary of canonical page §10 and §12)

- **Authority widening through extension points.** Hooks, Skills, MCP descriptions and repository files are untrusted.
  Only host policy grants authority. This ADR's hooks are deny-only, and ToolGuard stays final.
- **Duplicate side effects after a crash.** Effects are at-least-once until M5 receipts exist; nothing is blindly
  replayed. M2's crash matrix records current behaviour exactly.
- **Secret and continuation leakage.** Hook payloads exclude continuation state and credentials, and hook warnings never
  carry thrown text.
- **Second loop by accident.** The architecture test fails on any new `streamChat`/`execute` call site or Pi
  dependency.
- **Consumer breakage.** Changes are additive only, the golden traces must stay byte-identical, and `runChat`'s
  signature is frozen.

## Alternatives considered

- **Embed Pi `Agent` as the coding engine and keep `runChat` for chat.** Rejected: two loops, two histories, and
  recovery split across them (canonical page §1).
- **Nest Pi inside a `chat.turn` executor.** Rejected for the same reason. It also hides Pi's tool calls from
  `ToolGuard` and the event log.
- **Give `runChat` hooks directly.** Rejected. It changes the frozen facade's surface, and `runChat` callers would then
  depend on hook semantics that M2 and M5 will extend.
- **Allow hooks to rewrite arguments in M1.** Rejected. It needs the M5 invocation broker and threat model first.

## Consequences

- `runChat` behaviour is frozen. New capability arrives through `runAgentLoop` options and host configuration.
- Hook authors get observability plus a veto, not control. Control arrives with the governance work in M5.
- The `run-loop.ts` monolith is split into model and tool steps (M1a), which makes M2's steering and suspend seams
  local changes.

## Out of scope (deliberate)

- OneMind changes of any kind (M8).
- Parallel tool execution, token-aware context and compaction, and the coding tool pack.
- A public `1.0`, version bumps, tags or publishing.

## Verification

- M0: [docs/baseline/m0-2026-10-27.md](../baseline/m0-2026-10-27.md). Full gate green on `fed11f1`, golden SHA-256s,
  loop benchmark.
- M1: the gate plus byte-identical golden traces plus `packages/core/tests/agent-loop-hooks.test.ts`,
  `packages/host/tests/turn-runner-loop-hooks.test.ts`, `packages/testing/tests/architecture.test.ts`, and
  `bun scripts/bench-run-loop.ts` p50 within ±15% of the M0 numbers.
