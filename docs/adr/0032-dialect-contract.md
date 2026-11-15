# ADR 0032: Dialect contract

Status: accepted (2026-11-15). Implements TU-23 and TU-24 of the native tool
use spec (package T3.1). Builds on ADR 0028 (frozen manifest), ADR 0030 (model
profile) and the call pipeline of T1.6.

## Context

Models call tools best in the shape they were trained on. Qwen, Claude Code
and Gemini's CLI share one coding shape (`file_path`, `old_string`, `edit`,
`glob`, `run_shell_command`, flat git tools); AgentKit's own ids and guards
are built around canonical tools (`edit_file`, `find_files`, one
`git_inspect` with a `command`). Presenting a different shape per model must
not change what guards, approvals, receipts and stored data see.

## Decision

1. **A dialect is data plus two pure functions** (`agentkit_core::Dialect`).
   The data is a versioned table: per canonical tool, one or more wire tools
   (name, offered parameters, fixed canonical arguments, description
   override). `render(canonical definitions) -> presented definitions` and
   `translate(wire name, args) -> canonical name, args`. Neither touches
   state; `wire_call` is the inverse of `translate`.
2. **Common v1** is the default and the only table so far: `edit_file ->
   edit`, `list_dir -> list_directory`, `find_files -> glob`, `code_search ->
   grep_search`, `run_command -> run_shell_command`, and `git_inspect` split
   into `git_status`, `git_diff`, `git_log`, `git_show` (each flat; each
   translates back to `git_inspect {command}`). The canonical schemas already
   carry the common parameter names (T1.3), so no parameter is renamed.
   `read_file`, `write_file`, `run_tests`, `repo_map`, `submit_plan` and every
   non-coding tool pass through unchanged. Names that other descriptions
   mention are rewritten to the names the dialect offers.
3. **Selection.** A new session uses its frozen profile's dialect (`common`
   without a profile). `anthropic` and `openai` are stub entries that present
   the common forms under their own id until T3.2 and T3.3 (X3 may remove
   them). An unknown id or version falls back to `common`. A session whose
   manifest was frozen before dialects existed (manifest present, no dialect
   version in the prefix) keeps canonical names.
4. **Presentation sits before the manifest freeze.** The chat prefix renders
   the registry's definitions through the dialect; the manifest, the deferred
   catalog, the S2 index and the tools digest hold the presented names. The
   prefix records the dialect version; the registry keeps the dialect with the
   frozen surface (`freeze_presented`).
5. **Translation is the first pipeline stage**, before resolve and normalise
   (`call_pipeline::translate_call`, also used for the inner call of
   `tool_call`; `ToolRegistry::translate` is the public entry). Guards, hooks,
   receipts and approvals receive the canonical call, as today.
6. **Stored data keeps canonical names.** The assistant record stores the
   translated call; each request shows history through the dialect
   (`present_history`), so the model reads the names it was offered. Events
   (`run.tool.running`, results) carry canonical ids. The tool-call record
   keeps `toolName` as emitted, adds `canonicalTool`, `dialect` and
   `dialectVersion`.
7. **Failures name tools as the model sees them** (closest names, withheld,
   deferred, unavailable). A call a dialect cannot translate (a fixed argument
   contradicted, arguments not an object) is refused with `bad_args` before
   any guard.
8. **No widening.** A wire tool maps to exactly one canonical tool and fixed
   arguments; the dialect never adds a value the model did not choose except
   the fixed one the wire name states. A conflicting value is refused.
9. **Collisions are reported.** Two tools offered under one name (a foreign
   tool named like a wire name) raise a `tool_name_collision` warning; the
   test `common_v1_table_matches_the_spec_and_has_no_collisions` fails the
   build for the table itself.

## Consequences

- TU-23 is a test over the real coding pack: for every tool, example, mode and
  role the translated call equals the canonical call and the coding permission
  guard answers identically (`runtime/tests/dialect_equivalence.rs`).
- Changing a dialect table is a version bump: a running session keeps the
  version in its prefix.
- Profile `directTools` lists still name canonical ids; the deferred
  classification compares names in the manifest (wire names). Reconciling the
  two is a follow-up for the toolset package (T4.1).
- Constraint teaching errors still use canonical parameter wording (for
  example `git_staged_needs_diff` says `command "diff"`); with flat git tools
  the model rarely reaches them.

## Addendum T3.3: the OpenAI dialect (TU-26)

Vendor formats read 2026-11-15 (details in `docs/evidence/tool-use/T3.3`).

1. **Two built-ins, typed.** `apply_patch` (`{"type":"apply_patch"}`) stands for
   the canonical `write_file` and `edit_file`; `shell`
   (`{"type":"shell","environment":{"type":"local"}}`) for `run_command`. Both
   are `ToolDefinition`s carrying the `x-builtin-tool` marker (as in the
   Anthropic dialect); the Responses request builder sends them as typed
   tools outside the `agentkit` namespace. The patch tool is offered when
   either canonical tool is registered, the shell tool when `run_command` is.
2. **One canonical call per vendor call.** An `apply_patch_call` names one
   file, so the "several files" case does not occur. A V4A `update_file` with
   more than one hunk is refused with a message to send one hunk per call: an
   adapter tool that runs several canonical calls under one result would need
   the same guard verdict per hunk and one receipt per mutation (R-CNT-15),
   which this package does not prove. `create_file` -> `write_file`,
   one-hunk `update_file` -> `edit_file` (old = context + removed lines, new =
   context + added lines, header text after `@@` is a locator and dropped).
   `delete_file` is refused (no canonical delete). A shell call with several
   commands is refused for the same reason; `max_output_length` is dropped,
   `timeout_ms` -> `timeout`.
3. **Results in the vendor shape.** The call arrives as the vendor item's
   payload (`operation`, `action`) and the result is returned as
   `apply_patch_call_output` (`status` completed or failed, `output` the
   envelope summary) or `shell_call_output` (one entry; `run_command` merges
   both streams, so stderr is empty; a call without an exit code is exit 1
   with the failure text as stderr). A refused built-in call keeps its wire
   name in history so call and output pair.
4. **Strict schemas.** Every non-built-in tool is rendered strict where its
   schema allows: all properties required, optional ones nullable
   (`type` gains `null`, `enum` gains `null`), `additionalProperties:false`;
   `default`, `minLength`, `maxLength`, `uniqueItems`, `title`, `format` are
   left out of the model's view (the canonical tool still enforces them).
   A schema strict mode cannot take (open object, `oneOf`, unknown keyword)
   is offered as it is, non-strict. The request builder sets `strict` only
   under a strict dialect profile and only where the schema is strict.
5. **Null is absent.** For a strict dialect the registry removes a `null`
   argument from the canonical call when the canonical schema does not
   require it or accept null; the call is recorded as changed. A required
   argument sent as null is left for validation to refuse.
6. **Lanes.** `openai` is the Responses variant. `openai-chat` (common shape,
   strict schemas, no built-ins) is the Chat Completions variant; the host
   picks it for that lane. Wiring the lane into `session_dialect` is open.
7. **X3 not run.** Whether the vendor tools beat the common shape for GPT is
   the experiment that needs go-ahead; v1 is data, so the result can swap it.
