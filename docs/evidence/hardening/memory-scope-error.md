# Memory scope error names the available scopes

Added during the 18 Nov live OpenAI evaluation. The fix is B6-like: a teaching error (TU-10: what, why, next step).

## Finding

The OneMind baseline runs `acceptance-runs/2026-11-17T2302{15,18,21,24}Z-eval` (OneMind @4112ae5d) show 12 rejected `memory_propose` calls with `scope: "project"` in chats that have no project (gpt-6-luna 5, gpt-6.2-sol 4, gpt-6.2-astra 3). Each cost one extra provider request, out of a turn budget that is still capped at 4 (B2). The error read: "This session has no such scope to write to; use user, or the project or agent the session belongs to."

## Change

`rust/crates/memory/src/tools.rs`, `propose`: the error now names the missing scope and lists the scopes this chat can use. Example: "This chat has no agent scope to write to. Scopes available here: user, project. Retry with one of them." The code (`invalid_arguments`) and the validation are unchanged; only the message changes.

## Test that failed before

`agentkit-testkit` `tests/memory_tools.rs::a_missing_scope_error_lists_the_scopes_this_chat_can_write`. Before the change it failed at the `no agent scope` assertion; after the change it passes.

## Commands run

| Command | Result |
|---|---|
| `cargo test --locked -p agentkit-memory -p agentkit-testkit -p agentkit-eval` | all pass |
| `cargo clippy --locked -p agentkit-memory -p agentkit-testkit --all-targets --all-features -- -D warnings` | no issues |
| `cargo fmt` | clean |

Not verified: the live effect, which comes with the post-fix OpenAI rerun.
