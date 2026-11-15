# Package T3.3: OpenAI dialect and strict schemas (TU-26)

Tests written, NOT run (validation deferred to the owner). X3 (vendor tools vs common shape) NOT run: needs go-ahead; blocked.

## Vendor format, read from current docs (fetched 2026-11-15)

Sources (platform.openai.com/docs/guides/* 301-redirects to developers.openai.com/api/docs/guides/*):
- https://developers.openai.com/api/docs/guides/tools-apply-patch
- https://developers.openai.com/api/docs/guides/tools-shell
- https://developers.openai.com/api/docs/guides/function-calling
- https://developers.openai.com/api/docs/guides/structured-outputs (supported-schema subset)

Patch tool:
- Tool: `{"type": "apply_patch"}`.
- Call item `apply_patch_call`: `id`, `type`, `status` (docs show `completed`), `call_id`, `operation` = `{type: create_file | update_file | delete_file, path, diff}`; delete has no diff.
- Answer item `apply_patch_call_output`: `type`, `call_id`, `status` (`completed` | `failed`), optional `output` string.
- V4A: the docs do not define the grammar (the harness interprets it; reference code is `applyDiff` in the Agents SDKs). Example shows `@@` hunk start, `-` removed, `+` added, unprefixed context. Create: diff is the full file. We read a space or an empty line as context, drop text after `@@`.

Shell tool:
- Tool: `{"type": "shell", "environment": {"type": "container_auto" | "container_reference" | "local"}}`; we use `local` (we run the command).
- Call item `shell_call`: `type`, `call_id`, `action` = `{commands: [string], timeout_ms, max_output_length}`, `status` (docs example `in_progress`). The doc example has no item `id`.
- Answer item `shell_call_output`: `type`, `call_id`, `max_output_length` (when the call set one), `output` = array, one per command, `{stdout, stderr, outcome}`, `outcome` = `{type:"exit", exit_code}` | `{type:"timeout"}`.

Strict function schemas:
- Every property in `properties` must be in `required`; optional = `null` in the type (`["string","null"]`); `additionalProperties: false` on every object. Root must be an object, not `anyOf`. Not supported: `allOf`, `not`, `dependent*`, `if/then/else`; `minLength`/`maxLength`/`default`/`oneOf` are not in the supported list. Supported: `pattern`, `format` (limited), `minimum`/`maximum`/`multipleOf`/`exclusive*`, `minItems`/`maxItems`, `enum`, `anyOf`, `const`.
- Responses: omitting `strict` lets the API try to normalise and fall back to non-strict; Chat Completions is non-strict by default. We set `strict` explicitly: true only for strict-compliant schemas under a strict dialect profile, false otherwise.

## Assumptions the docs did not settle (not verified against the live API)

- Echoing `apply_patch_call` / `shell_call` items back as input (history, store=false) without `id`, with `status: completed`.
- A `shell_call` output item may lack `id`; the parser then keys it by `call_id`. Built-in call items may carry a status other than `completed` in the terminal response; the status check skips them.
- `max_output_length` of a shell call is not echoed in the output.

## What was built

- `core/src/dialect/openai.rs`: `openai` (Responses: patch + shell built-ins) and `openai-chat` (common shape) dialect data; render, translate (V4A -> `write_file`/`edit_file`, shell -> `run_command`), wire_call (history), `Dialect::strict`, `Dialect::built_in`.
- `core/src/dialect/strict.rs`: `strict_schema`, `is_strict_compliant`, `drop_nulls`.
- `core/src/dialect/mod.rs`, `lib.rs`: wiring and exports. `lookup("openai")`, `lookup("openai-chat")`.
- `core/src/registry.rs` (`translate`): null arguments of optional parameters dropped under a strict dialect. `core/src/call_pipeline.rs` (`translate_call`): uses the translated name (a built-in reaches several canonical tools), keeps a refused built-in under its wire name, and runs for strict dialects on every tool.
- `providers/src/responses_request.rs`: typed built-in tools, history as `apply_patch_call`/`shell_call` and their outputs, `strict` flag, continuation allow-list. `providers/src/responses_state.rs`: parses `apply_patch_call` and `shell_call` into calls whose arguments are the vendor payload. `providers/src/chat_completions_request.rs`: `function.strict` for strict schemas under a strict dialect profile.

## Tests written (not run)

- `core/tests/dialect_openai.rs` + `fixtures/dialect-openai-v1.json`: recorded vendor items, request tool names and strict schema, translate pairs and round trips, refusals, strict rewrite cases, nulls through the registry, history.
- `providers/tests/openai_builtin.rs`: request tools, whole loop with vendor items, failed output for a refused patch, strict flag, unknown marker.
- `runtime/tests/dialect_equivalence_openai.rs`: TU-23 for openai over the real coding pack: offered set, example round trips, nulls, same guard verdict in every mode and role, no widening.
- `core/tests/dialect.rs`: stub assertion skipped for openai.

## Known limits

- `canonical_of("apply_patch")` returns `write_file` (first form); only availability checks use it.
- `update_file` that removes the last line of a file with no final newline fails to match (the removed line's line end is part of what is replaced); the model gets edit_file's no-match teaching text.
- Lane choice: `session_dialect(prior, profile, lane)` takes a `ToolLane` (`ChatProvider::tool_lane`, Responses for `ResponsesProvider`, Chat Completions by default), so the `openai` profile yields `openai` on Responses and `openai-chat` on Chat Completions. Test `core/tests/dialect.rs::the_openai_profile_follows_the_provider_lane` (written, NOT run).
