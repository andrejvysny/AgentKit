# Package T1.6: call pipeline (AgentKit half)

Requirements: TU-08, TU-09, TU-12 (spec `06-native-tool-use-spec.html`).

Tests written, NOT run (validation deferred to the owner's pass):
- `core/src/call_pipeline.rs` unit tests: alias table (every spec row), scaling, canonical-absent rule, per-tool alias data, type coercion, never-fill-required, closest names.
- `core/tests/call_pipeline.rs`: canonical args reach the tool and the record shows aliases/conversions; unknown name lists 3 closest + tool_search; withheld tool; unnamed call; result order with a failing call.
- `providers/tests/call_faults.rs`: chat-completions and Responses, bad JSON / empty name / empty id each get a failure result; result order in the next request body.
- Anthropic parser fixtures live in OneMind (`stream_tests.rs`, `request_tests.rs`).

Design: `call_pipeline::prepare` runs resolve and normalise before the guard (`tool_gate`) and execution, so guards see the canonical call. Placeholder `UNNAMED_TOOL` carries a call whose name never arrived; synthetic ids are `call_{slot}` (providers emit a `tool_call_repaired` warning). Failure codes: `tool_missing`, `tool_not_allowed`, `tool_name_missing`, `bad_args`, `schema_invalid` (type unification is T1.7).
Unverified: compilation, all tests, `rust/api/agentkit-core.txt` snapshot (needs regeneration).
