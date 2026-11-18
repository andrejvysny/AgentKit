# hard(child-budget): reject an impossible child budget

Problem: a model passed `budget.max_tokens` 2000-4000 to `subagent_spawn`; one child request needs about 20k tokens, so the child failed at once with `subagent_budget_exhausted: child token budget cannot fit the request (0 used + 20806 requested > 2000)`.

Fix: `Brief::check_token_floor` (brief.rs) refuses an explicit `budget.max_tokens` below `SUBAGENT_TOKENS_FLOOR` (32,000) with an `invalid_brief` error naming the minimum and suggesting to omit `budget`. Called by the `subagent_spawn` tool right after `Brief::from_input`. Nothing is raised silently; defaults and ceilings unchanged.

Scope choice: model-facing tool path only. `SubagentService::spawn` (host API) is unchanged because `token_budgets.rs::a_child_cap_of_one_token_refuses_the_first_request_before_dispatch` pins the hard gate with a 1-token cap; that guard test stays intact.

Tests (failing first): `token_budgets.rs` floor tests (compile error: no `check_token_floor`), `subagents.rs::spawn_tool_refuses_a_child_token_budget_below_the_floor` (failed: tool accepted 2000).
