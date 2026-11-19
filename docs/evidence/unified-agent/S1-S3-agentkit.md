# S1 to S3, AgentKit half (spec 10)

Spec: `agent-codebase-analysis/specs/10-unified-agent-spec.html`. Sections: "Sessions, profiles and bindings", "Permission model", decision U17. This covers the AgentKit parts only. OneMind's `build_executor`, the `onemind-chat` profile and the descriptor work are separate.

## What changed

### S1: rename (U17)

- `agentkit_chat::ChatExecutor` is now `AgentExecutor`, and `ChatExecutorOptions` is now `AgentExecutorOptions` (`rust/crates/chat/src/chat.rs`, `chat/hooks.rs`). Every use in the workspace is renamed: crates, tests, examples, `rust/README.md`, `docs/architecture.md`, `docs/rust/*.md`, `rust/examples/egui-host/README.md` and a comment in `scripts/live/compaction-session.ts`.
- `#[deprecated]` aliases `ChatExecutor` and `ChatExecutorOptions` live in `rust/crates/chat/src/lib.rs`. `mod chat` is private, so the crate root is the only public path the old names ever had. The `agentkit` facade re-exports `agentkit_chat as chat`, so it carries both names without a change.
- Not renamed: the crate `agentkit-chat`, `run_chat`, `ChatSession`, `ChatRequest`, and the option fields and methods. CHANGELOG history, ADRs and earlier evidence keep the old name.
- `rust/api/agentkit-chat.txt` was edited by hand. The `AgentExecutor` blocks moved to their sorted place, and two `pub type` alias lines were added.

### S2: profile toolsets and contextPolicy

- `rust/crates/sessions/src/profiles.rs`: an `AgentProfile.tools` entry can be an exact name, `toolset:<id>`, or a prefix ending in `*`. `filter_tools` uses one matcher in three places: registered tools, withheld tools, and the `inherit_metadata` keep closure. A tool's toolset comes from `ToolRegistry::toolset_of`, which checks the tool's declaration, then a toolset's member list, then the `mcp__alias` namespace. The supervisor and reviewer class filters still run after a match, so a pattern cannot get past a role ceiling. Validation rejects `toolset:` with no id, a bare `*`, and `*` anywhere except the end.
- `rust/crates/core/src/registry.rs`: new `ToolRegistry::toolset_of_definition(&ToolDefinition) -> String`, which applies the same rule to withheld tools. `toolset_of` returns `None` for those.
- `AgentProfile.context_policy: Option<ContextPolicy>` (`contextPolicy`, skipped when absent, so existing digests do not change). `ContextPolicy { masking, auto_compaction }` uses camelCase and denies unknown fields. `merge` deep-merges `contextPolicy` the way it merges `limits` and `resources`.
- `rust/crates/chat/src/chat/context_budget.rs` `resolve_context_assembly` takes the turn profile's policy. It uses the policy's `masking` only when the descriptor's `context.masking.enabled` is unset, and its `autoCompaction` only when `context.autoCompaction` is unset. The executor's `auto_compaction` stays the last fallback, in `chat.rs`. A legacy `toolResultElision: false` is already normalised to `masking.enabled: false` in the descriptor, so a profile cannot turn masking back on for that session.
- API baselines edited by hand: `rust/api/agentkit-core.txt` and `rust/api/agentkit-sessions.txt`.

### S3: read-only command class

- `rust/crates/toolpacks-coding/src/command_policy.rs`: new `CommandClass::ReadOnly` (`"read_only"`). `classify_shell_command` runs in this order: the high-risk check, the plain-character check, the leading-assignment check, the read-only allow-list (`ReadOnly`), the existing allow-list (`Eligible`), and otherwise `Unknown`.
- Allow-list: `git` with `status`, `diff`, `log`, `show`, `rev-parse`, `ls-files` or `blame`; `git branch` with only `-a`, `-r`, `-v` or `--list`; `ls`, `cat`, `head`, `tail`, `wc`, `rg`, `grep`, `pwd`, `stat`, `file`, `tree`, `du` and `which`; and `find` without `-exec`, `-execdir`, `-delete`, `-fprint`, `-fprint0`, `-fprintf`, `-fls`, `-ok` or `-okdir`.
- Flags this change refuses beyond the brief, because they write a file or run a program: `--output*` on git, `--pre` on `rg`, `-o` on `tree` (alone or combined with other short flags), and `-C` or `--compile` on `file`.
- `git branch` never reaches the read-only check. `branch` is on the high-risk git word list, and that check runs first.
- Standing approvals (`coding_standing_approval_key`) cover the same commands as before. Those are the `Eligible` commands plus any `ReadOnly` command that the build and test allow-list also names, such as `git status`. `ls`, `cat` and the other new read-only commands get no standing approval.
- `rust/crates/toolpacks-coding/src/guard.rs`: in `read_only` and `plan` mode, a `run_command` call whose live `command` classifies `ReadOnly` is `Allowed`. The role ceiling is checked before this and still wins. A name on the plan-tool list keeps the plan path. A call whose arguments cannot be read keeps the refusal. `decide` and every other tool and mode are unchanged.
- API baseline edited by hand: `rust/api/agentkit-toolpacks-coding.txt` (`CommandClass::ReadOnly` under both paths).
- Tests that changed on purpose: the `git status`, `git diff HEAD`, `git log --oneline -n 5` and `git show HEAD` cases moved from `ELIGIBLE` to the new `READ_ONLY` list, and `ls` and `cat README.md` moved from `UNKNOWN` to it.

## Tests added

- `command_policy.rs`: `read_only_commands`, `near_misses_are_not_read_only` (find with `-exec`/`-delete`/`-fprint`, `git push`, `git branch`, `cat x > y`, `--output`, `rg --pre`, `tree -o`, `file -C`, `LANG=C ls`, `npm install`), and `standing_keys_cover_exactly_the_build_and_test_allowlist`.
- `guard.rs`: `read_only_and_plan_modes_run_read_only_commands_only`. In `read_only`, `git status` is allowed and `npm install` is refused. In `plan`, `git diff HEAD` is allowed and `npm install` is refused. In `ask`, `git status` is still asked. A supervisor is still refused by the role ceiling. `run_tests` is still refused.
- `profiles.rs`: `filter_tools_resolves_toolsets_and_prefix_globs`, `tool_entries_outside_the_grammar_are_rejected` and `context_policy_is_optional_and_left_out_of_the_digest_when_absent`.

## Verification (19 Nov)

Run from `rust/` with a private `CARGO_TARGET_DIR`.

- `cargo check --workspace --all-targets --locked`: ok. Two pre-existing `dead_code` warnings in `agentkit-embed-local` `src/pooling.rs` (`pool_tokens`, `finish` are only used under the `onnx` feature); that crate is untouched.
- `cargo fmt --all`, then `cargo fmt --all --check`: ok. rustfmt reformatted `chat/tests/{chat_failure,chat_typed_json,deferred_tools,session_model_profile}.rs`, `http/tests/{chat_continuation,chat_continuation_matrix/support,http,http_selection}.rs`, `runtime/src/builder.rs`, `sessions/src/profiles.rs` and `toolpacks-coding/src/command_policy.rs`.
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` (the `scripts/gates.mjs` form): ok. Without `--all-features` the only errors are the two `embed-local` warnings above; `cargo clippy --workspace --exclude agentkit-embed-local --all-targets --locked -- -D warnings` is ok. No code in the workspace uses the deprecated `ChatExecutor` aliases.
- `cargo test --locked -p agentkit-sessions -p agentkit-toolpacks-coding -p agentkit-core -p agentkit-chat --no-fail-fast`: 96 test binaries, 1146 passed, 0 failed, 6 ignored. The first run failed `profiles::tests::filter_tools_resolves_toolsets_and_prefix_globs` with `InvalidProfile("supervisor may not list non-read tools")`: its fixture extended `chat`, and the `supervisor` validation (present before this change) rejects a profile that lists `run_command`. The first three assertions had passed. The fixture now extends `coding` (implementer); every assertion is unchanged.
- `cargo test --locked -p agentkit-agents -p agentkit-runtime -p agentkit-store -p agentkit-http --no-fail-fast` (the other crates the rename touched): 83 test binaries, 833 passed, 0 failed, 3 ignored.
- `node rust/scripts/public-api.mjs` (check mode): `public-api: ok`. The hand-edited baselines match `cargo +nightly public-api` exactly, so `--update` was not needed.
- `node rust/scripts/check-arch.mjs`: ok (27 crates). It first reported `R12 agentkit-chat src/chat.rs` (1,002 lines by `wc -l`, over the 1,000-line limit). Comments added in `chat.rs` were shortened; no code moved.
