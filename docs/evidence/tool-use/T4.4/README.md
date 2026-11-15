# Package T4.4: recommender

Requirement: TU-29. Validation is deferred to the owner: these tests are written, NOT run.

## Built

`rust/crates/core/src/recommender.rs`: lexical scorer (diacritics folded, id,
title, keywords, tool names, card, descriptions, guide), usage prior
(`UsageStore` port, `InMemoryUsage`, `record_usage` from `run.tool.record`),
combined rank, one-line `Likely useful toolsets: a, b (load with toolset_load)`
appended to per-turn user-side context (never S1-S4), `ToolsetScorer` seam for
the vector term (roadmap 3.5), promote-top option default off.

## Tests written

`rust/crates/core/tests/recommender.rs` with fixture
`tests/fixtures/recommender-eval.json` (35 labelled requests, 11 Slovak):
top-two hit rate >= `TOP_TWO_HIT_RATE_THRESHOLD` (0.8, a proposal until X7);
diacritics; line format; usage reorders but never recommends alone; usage
event counting; promotion off by default; scorer seam; no toolset removed.

## Turn-context hook

`agentkit_chat::TurnContextHook` (port, like `DelegationGuidance`) is set on
`ChatExecutorOptions::turn_context`. Its line is appended to the turn's last
user message only (not persisted, never S1-S4); resumes that end in a tool
result get none. `ToolsetRecommendation` is the default implementation
(core `Recommender` over the session toolset view, `InMemoryUsage` fed from
`run.tool.record` events). Tests: `chat/tests/turn_context.rs` (written, NOT run).
