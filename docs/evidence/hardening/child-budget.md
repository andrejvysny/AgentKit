# hard(child-budget): reject an impossible child budget

Problem: a model passed `budget.max_tokens` 2000-4000 to `subagent_spawn`; one child request needs about 20k tokens, so the child failed at once with `subagent_budget_exhausted: child token budget cannot fit the request (0 used + 20806 requested > 2000)`.

Fix: `Brief::check_token_floor` (brief.rs) refuses an explicit `budget.max_tokens` below `SUBAGENT_TOKENS_FLOOR` (32,000) with an `invalid_brief` error naming the minimum and suggesting to omit `budget`. Called by the `subagent_spawn` tool right after `Brief::from_input`. Nothing is raised silently; defaults and ceilings unchanged.

Scope choice: model-facing tool path only. `SubagentService::spawn` (host API) is unchanged because `token_budgets.rs::a_child_cap_of_one_token_refuses_the_first_request_before_dispatch` pins the hard gate with a 1-token cap; that guard test stays intact.

Tests (failing first): `token_budgets.rs` floor tests (compile error: no `check_token_floor`), `subagents.rs::spawn_tool_refuses_a_child_token_budget_below_the_floor` (failed: tool accepted 2000).

## Follow-up commit

`529d6ceb` broke the static tool-schema lint: `agentkit-runtime` `tool_schema_lint::schema_rules_l1_to_l12_hold_for_every_static_tool` reported "subagent_spawn: L8: $.budget.max_tokens mentions parameter budget in prose but no constraint lists the rule". The description named `budget` in prose. It now says "At least 32000; leave it out to use the default." The lint passes (12 tests), as do `subagents::spawn_tool_refuses_a_child_token_budget_below_the_floor` and `public-api: ok`.
