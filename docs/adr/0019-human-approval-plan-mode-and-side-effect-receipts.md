# ADR 0019 — Human approval of tool calls, plan mode with a durable grant, and side-effect receipts

**Status:** accepted, implemented (2026-10-27; milestones M5 and part of M7 of [ADR 0015](0015-unified-agent-runtime.md)).
**Contract impact:** additive. `run.waiting` gains optional `kind`, `argsSha256` and `requestedMode`;
`AgentSuspendRequest` mirrors them. `CONTRACT_VERSION` and sqlite `SCHEMA_VERSION` are unchanged — nothing that
existed changed meaning, and an old client ignores the new fields. The Rust schema bundle and both fixture mirrors
were regenerated (`bun scripts/export-rust-fixtures.ts && bun rust/crates/contracts/codegen/generate.mjs`).

## Problem

ADR [0018](0018-coding-tool-pack.md) gave the coding agent confined tools and a guard that can say no. Saying no was
the whole vocabulary: `ask` mode refused a write and the model had to give up, and a refusal looked, in the durable
log and to the model, exactly like a crash. Three things were missing.

1. **A human in the loop.** A tool refusal that a person could overrule needs to be a *durable* request, not a
   prompt in a console: it must survive a restart, be answerable exactly once, and be bound to the exact call the
   human read. Otherwise "approved" is a rumour the model can repeat until something sticks.
2. **Plan mode.** A coding agent that can ask per call can also nag its way through a door one small edit at a
   time. Reviewers want a mode where the agent reads, proposes, and stops — with escalation being a plan a human
   approves as a whole, and the plan written by the model. That last part is the problem: a runtime whose escalation
   depends on what the model asked for has no escalation control at all.
3. **"It may have run".** A process that dies between a command starting and its result being recorded leaves a log
   with `run.tool.running` and nothing after it. Reconciling that hole as a plain failure tells the model the
   mutation did not happen, and a model that believes that writes the file again. The runtime must distinguish
   *never started* from *completed and recorded* from *may have completed and lost its acknowledgment*.

Requiring all three without a second source of truth is the constraint: every answer has to be reconstructible from
the store after a kill -9, because the alternative is a permission system whose state lives in a promise.

## Evidence

- Pi has no plan mode and no equivalent of (3): its coding-agent gate is a callback in the process
  (`OneMind/src-ts/src/code/pi-worker/gate.test.ts` covers the deny path, and OneMind's own gate is in-process too).
  Neither has durable approvals: restart loses what was pending.
- The approval machinery that already existed for `askHuman` (`human_request` inputs, `answerHumanRequest`, the
  parked-turn resume path) is exactly the shape needed for approvals — same request record, same single-answer rule,
  same terminal-state discipline. A second mechanism for "the human must answer" would be a second way to disagree.
- `ToolGuardVerdict` was `{ allowed: boolean; reason?: string }`. The refusal already carries a reason for the model;
  adding an `approval` capability to the refusal keeps the guard final (a refusal with no `approval` is still
  unsolvable by any human) while letting one say "ask".
- For (3), the durable event log is the only place that records a tool having begun: `run.tool.running` is appended
  with the attempt's fence before `execute()` is called. The conversation does not have it, and the tool result is
  exactly the record that is missing in the case that matters.

## Decision

### 1 — An approvable refusal (`packages/host/src/ports/tool-guard.ts`, `turn/registry-staging.ts`)

`ToolGuardVerdict` gains `approval?: { preview?: string; requestedMode?: string }`. A guard that refuses **with**
`approval` parks the call: the staged wrapper calls `ctx.suspend({ prompt, kind: "approval", argsSha256,
requestedMode })` and the turn goes to `waiting_approval`. Denial wins over approval — if any guard in the chain
refuses without `approval`, the call fails outright, so a stack of guards can never vote a hard refusal open.

The request is an ordinary `human_request` input whose content carries `kind: "approval"`, the `prompt`, and
`argsSha256`: the canonical-JSON hash of the arguments the human is being shown (`sessions/hash.ts`). Answering it is
`answerHumanRequest` with `{ decision: "approve" | "deny", note? }`, and the answer is validated against that hash
(`turn/approval-resume.ts`): a call whose arguments changed since the request was written fails with
`approval_mismatch` rather than executing something nobody read. An approval is spent once per tool-call id per
attempt, and a re-issued call goes back to asking.

Approved calls execute **before** the first model request of the resumed attempt (`AgentLoopInput.resumeCalls`,
`packages/core/src/runs/agent-loop.ts`). The human said yes to that operation; making the model ask again would
either re-run it unapproved or lose it, and it counts as step 1 without spending an iteration.

A denial is a recorded outcome, not a swallowed request: `{ ok: false, data: { errorCode: "approval_denied",
phase: "guard", retryable: false } }` is written as the tool result the model reads on the next turn.

### 2 — Plan mode and the grant the host writes (`sessions/plan-mode.ts`)

`plan` joins the coding modes. In it, reads run free and every write/exec class refusal is **final** — the guard's
reason is a hint at the way out (`plan mode: propose the change with submit_plan and wait for the user to approve
it.`), and no human answer can reopen those calls. The single approvable call is `submit_plan`.

Approving a plan writes an execution grant into chat metadata under `agentkit.session.execution`:

```ts
interface SessionExecutionState {
  version: 1;
  mode: string;                 // the effective mode; "plan" blocks writes
  grantedPlan?: PlanGrantRecord; // mode, planDigest, requestedMode, plan, requestId, turnId, approvedAt
  updatedAt: string;
  history: PlanGrantRecord[];    // most recent first, capped at 8
}
```

Three properties are the point of the design, and each is tested:

- **The grant is written in the same transaction that resumes the parked turn.** A crash cannot produce a session
  whose plan was approved and whose mode still blocks it, or the reverse. A partial write (answer marker committed,
  grant lost) is repaired on the next answer attempt from the durable request.
- **The host chooses the granted mode.** `PlanApprovalPolicy.grantedMode` is host configuration; the plan's
  `mode` argument is recorded as `requestedMode` and never obeyed. A plan asking for `full_auto` in a host configured
  to grant `auto_edit` gets `auto_edit`, and the human sees both. A plan may not grant `plan` — it is the escalation
  out of it, not a destination (`executionStateForGrant` throws).
- **A host veto runs before the transaction.** `PlanApprovalPolicy.authorize` can refuse the grant on
  role-ceiling or trust grounds; it is host code, so it runs outside the store's write lock.

Mode resolution for a session, in order: the session's durable execution state, then its profile's
`permissionMode`, then the host's configured floor. The model is not in that list.

### 3 — Side-effect receipts (`turn/side-effects.ts`, `turn/history-reconcile.ts`)

At turn start the runner reads the durable log for calls that began and never reported, across the current task and a
bounded number of the same scope's unfinished tasks:

```ts
interface UncertainCall { toolCallId: string; toolName: string; argsSha256?: string }
```

The scan follows log order and the last state of a call id wins: `run.tool.running` opens it, and
`run.tool.succeeded`, `run.tool.failed` or `run.waiting` close it. `run.waiting` counts because `run.tool.running` is
emitted before the guard chain runs, so a call the guard parked for approval logs "running" without having executed
(found by the live approval smoke, 2026-10-27). An approved call that then starts on the resumed attempt and dies is
open again, and is uncertain.

Only tools the staged registry declares `effect: "write"` (plus any the host names explicitly) are candidates; a read
that lost its result is a plain gap, not a hazard. Two consequences follow:

- History reconciliation writes the orphan's synthetic result with `errorCode: "tool_result_uncertain"` and text
  addressed to the model — *its side effects may already have been applied. Do NOT retry it as-is: inspect the current
  state with read tools, or ask the user before attempting it again* — instead of `tool_result_missing`. The envelope
  shape is otherwise identical to a real tool failure, so the model cannot tell a reconciled hole from a genuine
  error and start inventing an explanation for a difference it cannot see.
- A guard refuses any call identical to an uncertain one. It is indexed by **tool name plus the hash of its
  arguments**, not by tool-call id, because a model that repeats an interrupted command writes a new id for it. The
  refusal is approvable, not final: the human is the only party that can settle an unprovable exactly-once, and the
  approval machinery already binds an answer to one call and one hash.

The uncertain result is **not persisted** as a tool record. Inventing a durable record of an outcome nobody observed
would be a fabricated fact in the transcript; the model is told, in its own history, and the durable log keeps showing
what actually happened (`run.tool.running` with no result).

### 4 — Profiles enforce the same things from the other side

A profile ([ADR 0015](0015-unified-agent-runtime.md) §profiles) is host-side data, not a prompt: a tool allowlist
enforced at registry staging (`stageRegistry` prunes before the provider is shown anything, and the turn logs
`profile_tools_pruned` so a profile decision never reads like a contributor fault), limits that may only **lower** the
host's ceilings, a permission mode and role the model cannot move, and instructions — the only advisory part.
`createProfilePermissionAuthority` is how a profile reaches the coding guard, reading the durable execution state
first, so a plan approval outranks the profile and the profile outranks the fallback.

## Consequences

- **Approval is a fact in the store**, so any client — CLI, HTTP, desktop — can list what is pending and answer it,
  and a restart does not lose or invent a decision. The CLI in `examples/coding-agent`
  contains no approval state of its own because of this: it reads what the session is
  parked on and answers it.
- **`waiting_approval` is a first-class task state** in practice: the CLI polls it, the session service exposes
  `waiting`/`waitingApproval`/`waitingPlan`, and the crash matrix gains C5 (uncertain) as a covered case.
- **A guard now sees the running call**: `ToolGuardContext` carries `chatId`, `scopeId` and `toolCallId` at execution
  time, layered over the staging snapshot. A per-session permission policy that judged only the staging photograph
  would judge the wrong session — or none.
- **Every mutating tool must declare `effect: "write"` honestly**, because the uncertain classifier reads it. A tool
  that mutates and declares `read` is invisible to the guard; hosts that cannot trust a contribution (an MCP bridge,
  typically) name it in `mutatingToolNames` to override the declaration.
- **`uncertainSideEffects: false` restores the old behaviour** and is a documented escape hatch, not a default. It
  converts an unknown into "it failed", which is a lie with a retry attached.
- **Approvals need an input store** (`store.inputs`); a host without one gets refusals instead of approvals, which is
  the safe direction: no human available means no escalation.
- **The plan is not a sandbox.** Its text is model-written; what protects the workspace is the confinement and the
  ceiling that survive the grant. `grantedMode` bounds what an approval can ever enable.
- The standalone example (`examples/coding-agent`, ADR 0015 M7) is the executable proof of all four sections: it
  explores, edits, runs `bun test`, asks before mutating, honours a denial, gates writes behind an approved plan,
  continues a session in a new process, and survives a SIGKILL mid-command without silently re-running the command.
