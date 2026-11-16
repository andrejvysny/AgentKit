# Tool-use qualification

How to qualify a model for tool use through the production pipeline (R21,
TU-36). This replaces `conformance --live` as qualification evidence: that
runner sends string-only schemas with generic descriptions at temperature 0
through its own loop, so what it measures is not what a session offers.

## What runs

The driver is `rust/crates/runtime/tests/tool_use_qualification.rs` with
`tests/qualification/`. It is in `agentkit-runtime` because the run loop and
the lanes sit above `agentkit-providers` (ADR 0021); the scenario and assertion
library stays in `agentkit_providers::conformance`
(`production_scenarios`, `Scenario::judge`, `QualificationRecord`).

Per scenario and language it builds `AgentRuntime::builder` and uses:

- the coding toolpack registered on the coding lane, and the memory tools;
  schemas and descriptions are the real ones;
- the model's tool profile (`resolve_profile`), frozen into the session, so
  sampling comes from the profile and nothing forces temperature 0;
- the profile's dialect (the scripted model writes calls in wire form);
- guards, session execution mode, deferral and the durable run loop with its
  outcome classifier.

External effects are real ports with local backends: a temp-directory
workspace, the in-memory memory repository and index, an on-disk SQLite store
in the temp directory. Nothing else is replaced.

Scenarios (English and Slovak each, 12 runs): `read_file`, `edit_file`,
`search`, `multi_step_edit`, `refusal` (read-only session; the file must be
unchanged and no write may succeed), `memory_propose` (the memory tools are
deferred in the profiles, so the model uses `tool_search` then `tool_call`).
Pass or fail is judged on final state (file contents, memory records), on the
call sequence (expected calls in order, only offered tools, forbidden tools
never succeeding) and on the answer.

## Default run (no network)

```
cd rust && cargo test --locked -p agentkit-runtime --test tool_use_qualification
```

A scripted model plays each scenario, so the driver, the assertions and the
record are tested. This is not a model result.

## Live run (opt-in)

```
cd rust
AGENTKIT_LIVE_CONFORMANCE=1 \
AGENTKIT_LIVE_BASE_URL=https://litellm.example/v1 \
AGENTKIT_LIVE_MODEL=qwen3.8-27b \
AGENTKIT_LIVE_API_KEY=... \
AGENTKIT_LIVE_SERVER=litellm \
AGENTKIT_QUALIFICATION_OUT=../docs/qualification/records \
cargo test --locked -p agentkit-runtime --test tool_use_qualification live_qualification -- --ignored --nocapture
```

| Variable | Meaning |
|---|---|
| `AGENTKIT_LIVE_CONFORMANCE` | must be `1`, or the test skips |
| `AGENTKIT_LIVE_BASE_URL` | endpoint root; the operation (`chat/completions` or `responses`) is appended |
| `AGENTKIT_LIVE_MODEL` | model id sent to the endpoint and matched to a profile |
| `AGENTKIT_LIVE_API_KEY` | bearer key; optional; never printed |
| `AGENTKIT_LIVE_PROVIDER` | profile match kind, default `openai-compatible` (also `openai`, `anthropic`) |
| `AGENTKIT_LIVE_SERVER` | server behind the endpoint (`litellm`, `vllm`, ...); recorded only |
| `AGENTKIT_LIVE_PROTOCOL` | `chat-completions` (default) or `responses` |
| `AGENTKIT_LIVE_TIMEOUT_SECS` | per-scenario limit, default 300 |
| `AGENTKIT_QUALIFICATION_OUT` | directory for `qualification-<model>.json`, default `.` |
| `AGENTKIT_COMMIT` | commit to record; default `git rev-parse HEAD` |

The live run calls the endpoint through `ChatCompletionsProvider` or
`ResponsesProvider`, the production adapters; only the HTTP transport is the
driver's.

## The record

`qualification-<model>.json`, version `agentkit.tool-use-qualification/1`:
`model`, `provider`, `server`, `profile_id`, `profile_version` (the dialect
version the profile freezes), `profile_digest`, `schema_digest` (hash of the
tool definitions the model was offered), `prompt_digest` (hash of the system
text and scenario prompts), `toolsets`, per-scenario `pass`/`fail` with
failures, per-toolset rates, `qualified_toolsets`, `timestamp`,
`agentkit_commit`.

A toolset is qualified when success >= 0.90 and invalid-call share <= 0.05
(OD7, the same thresholds as the matrix). The record is valid only for the
exact tuple: `QualificationRecord::qualifies(&key, toolset)` is false when
model, provider, server, profile id/version/digest, schema digest, prompt
digest or toolsets differ. `agentkit_commit` and `timestamp` are recorded but
not part of validity. Re-run after any change to a tool schema or description,
the system prompt, the profile or the model.

Wiring `qualified_toolsets` into default exposure and promotion is the host's
job; hosts should treat a missing or non-matching record as unqualified.
