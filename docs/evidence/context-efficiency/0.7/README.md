# Package 0.7: usage mapping required by the OneMind UI

This package corrects the OpenAI normalization introduced in package 0.1 before
OneMind exposes the counters. Input classes must be disjoint: uncached input is
reported input minus cache reads and cache writes. Missing cache-write usage is
unknown, rather than an inferred zero. Positive aggregate writes without a
reported lifetime split leave the lifetime buckets unknown. A reported zero
aggregate establishes that both nonnegative write buckets are zero.

The current [OpenAI prompt-caching guide](https://developers.openai.com/api/docs/guides/prompt-caching)
documents Responses `input_tokens_details.cache_write_tokens` and its disjoint
`cache_write_tokens_details.ttl_30m_tokens` / `ttl_12h_tokens` split. The generic
short/long normalized fields represent these provider-specific lifetimes; raw
usage retains the actual names. Chat Completions maps its aggregate write count
without assuming that the Responses lifetime-detail shape applies to Chat.

Known partial counts provide lower bounds for validation, not invented values
for missing counters. Numeric contradictions and unsafe sums fail with
`malformed_usage`. Original provider usage remains available in `provider_raw`.
Legacy total counters remain compatible. Output/reasoning normalization is
unchanged: unreported reasoning does not establish non-reasoning output.

The owner-approved 0.6 snapshot remains pinned to package 0.4. OneMind adopts
this correction through its official vendor script in its own 0.7 commit.
No price estimate, provider request, model download or subscription charge is
inferred from these counters.

Focused qualification passed: 37 provider Rust tests, 17 TypeScript tests,
provider clippy, strict TypeScript and Biome. Added semantic regressions rejected
partial lower bounds above the input total in both implementations before their
fix. The final TypeScript run separates the lifetime, aggregate and unknown
cases into short tests without removing assertions. Independent final review
found no remaining actionable issue. Full local qualification passed all 18
required gates: 2,161 Rust tests across 243 binaries, 246 client/React tests,
759 native/core tests, formatting, clippy, dependency checks, architecture,
contracts, typechecking, API checks, native and package builds, and smoke.
The Rust suite used `RUST_TEST_THREADS=1` to isolate the previously observed
fixture deadline contention; no assertion, process deadline or guard changed.
The complete runner summary and substantive red/green logs are retained here.
The architecture allowlist and public API snapshots are unchanged.
CE-16's UI half belongs to the OneMind repository;
this correction supports its input without claiming the
later cache-stability gates.
