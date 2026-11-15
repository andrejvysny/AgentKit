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

## Not done

Wiring the line into the chat executor's turn context (host call:
`Recommender::recommend(..).append_to(&mut turn_context)`).
