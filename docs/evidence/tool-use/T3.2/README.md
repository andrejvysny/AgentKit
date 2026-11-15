# Package T3.2: Anthropic dialect (TU-25)

Tests written, NOT run (validation deferred to the owner). X3 (vendor tools vs common shape) NOT run: needs go-ahead; blocked.

## Vendor format, read from current docs (fetched 2026-11-15)

Sources (docs.claude.com 302-redirects to platform.claude.com):
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/text-editor-tool
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/bash-tool

Text editor:
- Tool entry: `{"type": "text_editor_20250728", "name": "str_replace_based_edit_tool"}`; no `input_schema`, no description. Optional `max_characters` (only `text_editor_20250728` and later; not set by us).
- Versions in the change log: `text_editor_20241022` (Sonnet 3.5, retired, name `str_replace_editor`, has `undo_edit`), `text_editor_20250124` (Sonnet 3.7), `text_editor_20250429` (removes `undo_edit`, name changed to `str_replace_based_edit_tool`), `text_editor_20250728` (current; adds `max_characters`). Supported models list includes claude-opus-5-5, sonnet-5-5, haiku-5-5, fable/mythos 5.x, opus-4-x, sonnet-4-x.
- Commands and input fields (all carry `command`):
  - `view`: `path`; optional `view_range` `[start, end]`, 1-indexed, end `-1` = to end of file; a directory path lists the directory (`"src/"` in the docs example); `view_range` applies to files only.
  - `str_replace`: `path`, `old_str` (exact match incl. whitespace), `new_str`.
  - `create`: `path`, `file_text`.
  - `insert`: `path`, `insert_line` (insert after this line, 0 = beginning), `insert_text`.
  - `undo_edit`: only in `text_editor_20241022` and `text_editor_20250124`; removed from `20250429` on. Not offered.
- Errors go back as `tool_result` with `is_error: true` (file not found, multiple matches, no match).

Bash:
- Tool entry: `{"type": "bash_20250124", "name": "bash"}` (name must be `bash`, schema-less). `bash_20241022` is Sonnet 3.5 only (retired).
- Input fields: `command` (required unless `restart`), `restart` (bool, restart the session).

## What was built

- `core/src/dialect/anthropic.rs`: Anthropic v1 data, render, translate, wire_call.
  - `render`: read_file, write_file, edit_file, list_dir fold into ONE built-in definition (`str_replace_based_edit_tool`), run_command into `bash`. A built-in is a `ToolDefinition` with empty description, no examples, and `input_schema = {"type":"object","x-builtin-tool":"<vendor type>"}`; `builtin_tool_type(&def)` reads the marker and a request builder sends `{type, name}`. The editor is offered only when read_file, write_file and edit_file are all registered (never a wider tool than the session has); otherwise those tools keep the common form. Search, git, plan tools: common shape.
  - `translate`: view -> read_file (`view_range` -> `offset`/`limit`; end -1 -> offset only); view of `.` or a path ending `/` -> list_dir; str_replace -> edit_file (`replace_all: false`); create -> write_file; bash `command` -> run_command. Unknown keys, bad `view_range`, missing fields are refused, never repaired.
  - Refused with teaching text (canonical tools cannot express them, nothing is invented): `insert` (canonical edit needs an anchor string, which only the file can give: use str_replace), `undo_edit`, bash `restart` (each command is a fresh shell; put cd/exports in the command).
  - `wire_call` (history): inverse of the above; a call the built-ins cannot say (edit `replace_all` true, run_command `directory`/`timeout`, list_dir `include_ignored`) stays canonical.
- `core/src/dialect/mod.rs`: `anthropic` module, dispatch by dialect id in render/translate/wire_call, `lookup("anthropic")`. `lib.rs`: exports.

## Tests written (not run)

- `core/tests/dialect_anthropic.rs` + `fixtures/dialect-anthropic-v1.json` (hand-built from the doc examples): recorded request/response, view_range cases, refusals, malformed calls, partial tool sets, history.
- `runtime/tests/dialect_equivalence.rs`: TU-23 equivalence and guard-verdict tests extended to anthropic (every expressible example, every mode and role), plus no-widening (supervisor refused for create/str_replace/bash; view stays read).
- `core/tests/dialect.rs`: stub assertion skipped for anthropic.

## Known limits

- `canonical_of("str_replace_based_edit_tool")` returns `read_file` (first form); it is used only for availability checks, which the all-three rule makes safe.
- Directory view with no `list_dir` registered fails as an unknown tool.
- X3 not run; v1 is data, so X3 can swap it without code change to callers.
