# ADR 0025 — Claude Code native backend for general Chat (execution contracts)

Status: Accepted (implementation in progress; AGENTKIT-107 scope, AGENTKIT-108 owner)
Date: 2026-11-07
Plane: AGENTKIT-107 / AGENTKIT-108 · consumer ONEMIND-336

## Context

OneMind must offer general-purpose Chat through the user's installed,
unmodified official Claude Code executable using its own authenticated
account, while AgentKit remains the single durable orchestration owner
(tasks, attempts, leases, public history, tool authority, effects,
cancellation, recovery). Claude owns the inference/tool-planning loop inside
its runtime. Each admitted turn selects exactly one execution owner.

## Decision

### 1. One durable host, one selected execution owner per run

`agentkit-engine` (tasks, attempts, leases) and `agentkit-store` remain the only
durable authority. A vendor session is a
binding, never a second task system. The generic `NativeAgentExecutor`
(AGENTKIT-113) lives in `agentkit-native-exec`; vendor protocol lives in the
optional driver crate `agentkit-claude`, which implements injected ports.
The executor never depends on any driver crate. The crate graph compiles with
the Claude feature disabled; no JS/Python inference runtime exists.

```
agentkit-native (contracts + ports; depends on nothing above it)
      ↑                ↑
agentkit-native-exec   agentkit-claude (optional driver)
      ↑                ↑
agentkit-rust facade (feature "claude" composes executor + driver)
```

### 2. Versioned execution target (wire contract)

`agentkit_native::ExecutionTarget` is an externally tagged enum with a
`schemaVersion` (=1) in every payload, `deny_unknown_fields` everywhere:

```json
{"modelLoop": {"schemaVersion":1,"providerId":"…","model":"…","reasoningEffort":"…"}}
{"native": {"schemaVersion":1,"backend":"claude-code","backendVersion":"2.1.310",
  "profile":"plainChat"|"ownedMcpChat","executable":"/abs/path/claude",
  "requestedModel":"…","requestedEffort":"…","authExpectation":"subscriptionLogin",
  "capabilities":{…snapshot…},"configIdentity":"sha256:…",
  "systemPromptRevision":"…","content":{…policy…}}}
```

* Frozen per admitted turn; settings changes affect future turns only.
* Absent target = the existing HTTP/model-loop interpretation (legacy
  compatibility, AGENTKIT-112 migration rule).
* No contract type carries credentials; a test asserts no credential-shaped
  field name appears in a serialized target.
* `NativeProfile::BareMode` is deliberately unrepresentable: bare mode
  cannot serve the subscription-auth path.

### 3. Version-qualified capability policy

`CapabilityMatrix` maps driver-owned version ranges → flags with three
states (`supported{qualifiedVersion,evidence}`, `unsupported{reason}`,
`unknown`). Admission runs `validate_execution_target` BEFORE any spawn:
unknown or unsupported required flags, image/file modalities without
evidence, profile/capability mismatches, and out-of-scope flags
(`native-subagents`, `codex`, `native-coding-tools`) are refused with stable
codes. `ModelLoopTarget` always passes — legacy behavior unchanged.

### 4. Separate event classes

`NativeEvent` (normalized data: init, text/reasoning deltas, complete
messages, vendor tool observations, usage, warnings, retry, permission
denials) is data only. `NativeEvent::ToolObservation` is never dispatchable;
only the governed MCP bridge (AGENTKIT-114) executes application tools.
`NativeTerminalOutcome` (completed/failed/deferredBlocked/interrupted/
uncertain) is the single authoritative turn result; `ProcessExit` (pid,
generation, exit code, signal) and stdout EOF are distinct signals. Exit
without a valid root result can never produce `Completed`.

### 5. Injected ports

* `NativeBackend` (driver): `preflight`, `plan_turn` (typed argv/env/cwd/
  per-run dir — prompts never in shell strings), `encode_input`,
  `new_frame_adapter`, `terminal_outcome`.
* `ProcessSupervisor` (host application or driver default): `spawn` with
  typed argv + explicit cwd/env + `ResourceLimits`; `drain_owned` for
  host-shutdown. Children are generation-fenced; only owned descendants are
  terminated; cleanup uncertainty is reported, never silent.
* `ExecutableLocator` (host application): canonical executable + official
  `claude auth status` evidence; probe and run share the official
  configuration namespace; credentials are never read.
* Interaction contracts (`PendingInteraction`, `AnswerSubmission`,
  `AnswerDisposition`, `InteractionRejection`) bind
  task/attempt/process-generation/invocation/content-hash/actor/expiry for
  the live (Running, leased) approval path; the parked `WaitingApproval`
  path is never used for a live child.

### 6. Launch policy (qualified controls, honest limits)

Plain Chat: authentication-preserving `--safe-mode`, `--tools ""`,
`--disallowedTools "mcp__*"`, `--permission-prompts none`,
`--disable-slash-commands`, `--no-chrome`, controlled cwd, private per-run
`--settings`, `CLAUDE_CODE_RESUME_INTERRUPTED_TURN` never set (ambient
interrupted-turn resume disabled). Tool Chat adds
`--strict-mcp-config --mcp-config <private file>` with only the scoped
AgentKit bridge server and exact preapproved broker names
(`mcp__agentkit__…`), while host-side governance still gates every
operation. Exact argv/env layout is owned by `agentkit-claude`
(`src/launch.rs`) and is part of the `configIdentity` digest.

## Consequences

* OneMind keeps `onemind.chat` and dispatches on the frozen target; the
  library never hardcodes a public task label.
* Storage (AGENTKIT-112) adds a non-destructive populated migration
  (schema step 17) for backend/session bindings and live interactions;
  legacy rows keep their HTTP meaning.
* Live qualification is separate from implementation: the standalone gate
  is AGENTKIT-118, the packaged OneMind gate is ONEMIND-346. Mocked
  evidence is never called live qualification.

## Verification (108)

`cargo test -p agentkit-native` — contract tests prove: pre-spawn rejection
of invalid capability combinations; legacy HTTP targets unchanged;
serialization round-trip with schema version; unknown-field refusal; no
credential-shaped fields; exact-resume-only wire (no continue-latest).

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
