# Package T1.3: lint and schema fixes, AgentKit

Requirements: TU-04, TU-05 (AgentKit side), TU-07 for AgentKit's renamed
parameters. Base: AgentKit `master` with the T1.6 and T1.7 working-tree changes
present. OneMind untouched.

## What was built

- `rust/crates/runtime/tests/tool_schema_lint.rs`: one lint over all 25 static
  AgentKit tool definitions (11 coding, 7 subagent, 1 advisor, 6 language
  server), rules L1 to L12, exception list empty by construction. It sits in
  `agentkit-runtime` because that crate may depend on all four sources (layer 5;
  `check-arch` allow-list untouched). A second test proves the lint is not
  vacuous (a definition that breaks each rule is flagged by each rule), a third
  compares the schema in the Chat Completions and Responses request bodies with
  the definition (L10), a fourth maps every renamed parameter through the
  pipeline and validates it.
- Contract: `ToolDefinition.examples` (core), `AiToolDefinition.examples`
  (contracts DTO, schema bundle, fixtures, manifest hash, TypeBox, native
  callback type). Optional, omitted when empty.
- `Tool::constraints()` / `Tool::check_constraints()` (L8) and
  `Tool::arg_aliases()` (per-tool old names, collected by `ToolRegistry::register`
  into the call pipeline's alias table).
- Coding pack: `ToolSpec` gained `examples`, `constraints`, `aliases`; the
  wrapper enforces constraints before the body, and keeps direct callers on the
  canonical names with the same `normalise` the pipeline uses, saying so in the
  result warnings.
- Subagent, advisor and language-server definitions are now public functions
  (`subagent_tool_definitions`, `advisor_tool_definition`,
  `lsp_tool_definitions`) and the tools are built from them.

## Schema changes

| Tool | Change |
|---|---|
| `read_file` | `file_path`, `offset`, `limit` only; no root either-or; `paths` stays parseable for direct callers, not in the schema |
| `write_file` | `file_path`, `content`; `expectedSha256` out of the schema |
| `edit_file` | `file_path`, `old_string`, `new_string`, `replace_all` (new, exact matches only); `edits[]` and `expectedSha256` out of the schema |
| `list_dir` | `path`, `include_ignored` |
| `find_files` | description only (already conformant) |
| `code_search` | 8 to 4 parameters: `pattern`, `path`, `glob`, `limit` |
| `repo_map` | `required: []`, bounds, description |
| `git_inspect` | flat: `command`, `path`, `staged`, `limit`, `rev`; three constraints |
| `run_command` | `directory`, `timeout` in ms (1000 to 600000) |
| `run_tests` | `command` is an enum of the configured commands |
| `subagent_*` | snake_case; `role` and `mode` enums; bounds; `spawnKey`, `messageKey`, `resumeKey` out of the schema |
| `lsp_*` | `file_path`, `include_declaration`; bounds; descriptions 150 to 900 |
| `advisor_review`, `submit_plan` | closed schemas, examples |

## Checks kept behind removed parameters

Removing a parameter from a schema did not remove its check:

- `expectedSha256` is still parsed (optional). The default check, the session's
  read ledger, is unchanged: `write_file` refuses an overwrite of a file this
  session did not read in full; `edit_file` refuses a stale base.
- `spawnKey` is still read when present; without it the key derives from the
  tool call id, and a call with no call id is still refused
  (`spawn_key_required`), now with a message that does not name the removed
  parameter. `messageKey` and `resumeKey` keep their derived defaults.
- The command guard (`shell_args_of`) now reads the directory from
  `directory`, `cwd` or `workdir`, and treats two at once as uncoverable, so the
  approval key matches the directory the tool will run in under every spelling.

## Not verified

Validation is deferred to the owner's pass. Nothing was compiled or run:
no `cargo build`, `test`, `clippy`, `check-arch`, `bun test` or `contracts:check`.
Only `rustfmt --check` (syntax and layout) was run on the files changed.
