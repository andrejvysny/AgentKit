# A child on its parent's model inherits the admitted context window

Added during the 18 Nov live OpenAI evaluation, for subagent quality (H4 area; owner priority).

## Finding

In the legacy capture run `acceptance-runs/2026-11-17T234202Z`, every child turn's `run.context.admitted` event reads `windowTokens 32768, windowSource fallback, reserveTokens 16384`, about 13k tokens of message budget, even when the parent model had 1,050,000 (for example the gpt-6-luna child `turn:subin:3e790df3c4e135e52e289f86`). Children then hit `subagent_budget_exhausted`, overflowed, and returned thin results.

Cause: the host admits the window per turn into the parent turn's payload (`contextWindowTokens`). `SubagentService` created the child session with the parent descriptor's `context` only, which names no window for a plain chat. The child's turns carry no admission, so `ContextBudget::resolve` reached `context_window::resolve` and its 32,768 fallback.

## Change

`rust/crates/agents/src/subagents/service.rs`: `child_context` copies the parent turn's admitted `contextWindowTokens` into the child descriptor's `context.windowTokens`, under three conditions:
- the child runs the same provider and model as the parent;
- the parent's context names no window;
- the admitted window is positive.

A role pinned to another provider or model never inherits it.

## Tests

`tests/provider_routing.rs`:
- `a_child_on_the_parents_model_inherits_the_admitted_window` failed before the change (null window) and passes after.
- `a_child_on_another_model_does_not_inherit_the_window` pins the guard.

## Commands run

| Command | Result |
|---|---|
| `cargo test --locked -p agentkit-agents` | all pass, 12 binaries |
| `cargo clippy --locked -p agentkit-agents --all-targets --all-features -- -D warnings` | no issues |
| `cargo fmt -p agentkit-agents` | clean |

Not verified: the live effect, which comes with the post-fix rerun.
