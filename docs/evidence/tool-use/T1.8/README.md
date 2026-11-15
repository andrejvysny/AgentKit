# Package T1.8: examples and description standard, AgentKit

Requirement: TU-13 (lint L11, L12). Tests written, NOT run (validation is the
owner's pass; no cargo was run).

## What was built

- `rust/crates/core/src/examples_render.rs`: pure `render_examples(def, lane)`
  and `presented_description(def, lane)`, lanes `ExampleLane::{Anthropic,
  AnthropicNative, ChatCompletions, Responses}`. T3.1's dialect layer calls
  these when it builds the tool list. The definition is never mutated, so L10
  (schema sent equals schema defined) still holds; only the description text is
  derived.
- Wired at presentation: `providers/src/chat_completions.rs` (description in
  the `function` object) and `providers/src/responses_request.rs`.
- Anthropic: the native field is `input_examples`, a beta feature (it needs the
  `advanced-tool-use` beta header). No code path in AgentKit or OneMind sets
  that header (the only `anthropic-beta` in the tree is a test value), so
  `ExampleLane::Anthropic` appends to the description. `AnthropicNative`
  returns `RenderedExamples::Native` for a caller that has sent the beta; it is
  unused until then. Unverified against the current Anthropic docs (no network).
- Block format: `\n\nExample: <wire name>({json})`, one line per example.
- Description standard: lint-only. `description_gaps` in
  `rust/crates/runtime/tests/tool_schema_lint.rs` checks three loose markers
  (when to use, when not or which tool instead, what it returns). It is
  report-only; the 13 tools listed in `STRUCTURE_COMPLIANT` may not regress.
  The authored 150-900 limit (L12) and the empty exception list are unchanged.
  The rendered length is printed per tool and is not limited.

## Tests written

- `core/tests/examples_render.rs`: per-lane snapshots (rendered length 140
  recorded), native lane, no-example tool, definition untouched.
- `runtime/tests/tool_schema_lint.rs`: `description_structure_is_reported_and_compliant_tools_keep_it`,
  `the_structure_check_flags_a_missing_part`,
  `presented_examples_leave_the_schema_and_authored_text_intact`.

## Input for T4.3 (non-compliant by hand reading; the report test is authoritative)

Run `cargo test -p agentkit-runtime --test tool_schema_lint -- --nocapture` and
read the `T1.8 structure gap` lines. By reading the source, these 12 lack a
part (most lack "returns"): list_dir, edit_file (not / instead), code_search,
repo_map, git_inspect, run_command, submit_plan, subagent_spawn (use / not),
subagent_list, subagent_status, subagent_cancel, subagent_resume. No wording
was changed.

Unverified: compilation, every test above.
