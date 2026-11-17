# agentkit-eval

Layer A of the evaluation plan: deterministic checks that the harness is
correct. No model, no network, no clock reads, no paid calls. A failure here
is a harness bug, never a model's bad day.

The crate **coordinates** infrastructure that already exists and does not copy
it: `agentkit-testkit` (scripted providers, `ManualClock`, context fixtures,
prefix harness), the provider conformance scenarios and judge
(`agentkit-providers::conformance`), the runtime production-pipeline driver
(`runtime/tests/qualification`) and the context capture harness
(`runtime/examples/context_eval`). The last two are compiled into the suites
with `#[path]`; they are not duplicated.

## Where it sits

`agentkit-eval` is classified layer 6 in `rust/scripts/check-arch.mjs`
(`LAYERS`), the same layer as the other crates that sit above
`agentkit-runtime`. Nothing depends on it. Its library has no dependency on
`agentkit-testkit` (R3); every dependency on testkit, runtime, store and the
contexts is a `dev-dependency` used by `tests/`. The substance is in `tests/`;
`src/` holds only helpers no suite can own alone. `arch-allowlist.json` is
unchanged.

| Module | Purpose |
|---|---|
| `src/http_fixture.rs` | `FixtureTransport`: serves `fixtures/http/*.sse` bytes to the real Chat Completions and Responses providers; records the request bodies |
| `src/run.rs` | drives the real `run_chat` loop and reduces its events to JSON; `RecordingTool` |
| `src/oracle.rs` | mini grader over a normalized trace, `Outcome {Pass, Fail, Unknown}` |
| `src/coverage.rs` | the invariant to existing-test map below, verified against the tree |

## How to run

```sh
cd rust
CARGO_INCREMENTAL=0 cargo test -p agentkit-eval            # everything
CARGO_INCREMENTAL=0 cargo test -p agentkit-eval --test tool_pipeline
CARGO_INCREMENTAL=0 cargo test -p agentkit-eval -- --ignored   # the recorded bugs
cargo clippy -p agentkit-eval --all-targets
node scripts/check-arch.mjs
```

Determinism: suites that run the loop use `flavor = "current_thread"`, an
injected `ManualClock` for ids and timestamps, and raw bytes from fixtures, so
the same bytes replay to the same events
(`tool_pipeline::the_same_bytes_replay_to_the_same_events`). Fault points are
explicit: a fixture is a fault (truncated JSON, no name, unknown tool, no
usage), never a random draw.

## Suites owned here

| Suite | Test file | What it proves |
|---|---|---|
| tool_pipeline | `tests/tool_pipeline.rs` | Raw SSE through the real Chat Completions and Responses parsers and the real run loop: every model-emitted call (valid, truncated JSON, empty name, unknown tool) gets exactly one `run.tool.record` and exactly one result in the next request, in call order; alias and text-to-number normalization is recorded; a direct call and the same call through `tool_call` get the same guard verdict, refusal and approval prompt |
| context | `tests/context.rs` | The runtime's 30-turn capture harness on both lanes and both profiles: the frozen prefix never breaks; rule and selection changes reach the model without an unannounced break |
| memory | `tests/memory.rs` | Pasted ranges, fences, quotes, over-long paragraphs, tool results, assistant text and a child's report never reach the extractor; a proposal is invisible to search and the core block until approved; a purge leaves no text in a real SQLite file, its FTS tables or its vector table |
| delegation | `tests/delegation.rs` | Role specs and the spawn snapshot: read-only roles name no write tool, no role names a memory write, widening a role after spawn never widens a child, narrowing does, a deleted role fails closed, a forged snapshot is refused |
| accounting | `tests/accounting.rs` | Each provider request settles exactly one final `run.usage`; a server that reports none gets none; counters the provider did not report are absent, never 0 |
| oracle_integrity | `tests/oracle_integrity.rs` | The grader rejects wrong-answer, missing-call, duplicate-write and altered-authority traces, answers `Unknown` for incomplete evidence (exhaustive over the four evidence flags: only all-true passes), and accepts a known-good trace; against the production pipeline it agrees with the conformance judge on real and corrupted runs |
| coverage | `tests/coverage.rs` | Every test named in the table below still exists, and the table in this file is the map |

## Invariant to existing test

Where an invariant already has an owner, the owner is named instead of being
re-implemented. `tests/coverage.rs` fails if a named test disappears or if
this table drifts from `agentkit_eval::coverage::OWNERS`. The table is
`coverage::markdown_table()`; paths are relative to `rust/crates`.

| Suite | Invariant | Owning test |
|---|---|---|
| tool_pipeline | results of one turn come back contiguous and in call order | `core/tests/call_pipeline.rs::results_of_one_turn_are_contiguous_and_in_call_order` |
| tool_pipeline | a call without a name still gets a result | `core/tests/call_pipeline.rs::unnamed_call_gets_a_result_that_says_the_name_was_missing` |
| tool_pipeline | an unknown tool name gets a teaching failure | `core/tests/call_pipeline.rs::unknown_name_lists_three_closest_names_and_points_to_tool_search` |
| tool_pipeline | aliases and text values are normalized and recorded | `core/tests/call_pipeline.rs::alias_and_text_values_reach_the_tool_canonical_and_are_recorded` |
| tool_pipeline | one run.tool.record per call, with an outcome class | `core/tests/tool_records.rs::every_tool_call_produces_one_record_with_an_outcome_class` |
| tool_pipeline | tool_call meets the same guards as the direct call (allowed) | `chat/tests/deferred_tools.rs::ce08_an_allowed_call_runs_identically_through_tool_call` |
| tool_pipeline | tool_call meets the same guards as the direct call (refused) | `chat/tests/deferred_tools.rs::ce08_a_refusal_is_the_same_refusal_through_tool_call` |
| tool_pipeline | an approval binds the inner tool and its arguments | `chat/tests/deferred_tools.rs::ce08_an_approval_names_the_inner_tool_and_binds_the_inner_arguments` |
| tool_pipeline | a dialect call gets the same guard verdict as the canonical call | `runtime/tests/dialect_equivalence.rs::the_translated_call_gets_the_same_guard_verdict_as_the_direct_call` |
| tool_pipeline | a dialect never widens authority | `runtime/tests/dialect_equivalence.rs::a_dialect_never_widens_authority` |
| context | sixty tool-heavy turns break the cache only at mask points (cache_break per mask) | `chat/tests/batched_masking.rs::sixty_tool_heavy_turns_break_the_cache_only_at_mask_points` |
| context | masked results stay readable through read_output | `chat/tests/batched_masking.rs::masked_results_stay_readable_through_their_handle` |
| context | a result quoting another call's handle gets its own output | `chat/tests/batched_masking.rs::a_result_quoting_another_calls_handle_gets_its_own_output` |
| context | a compaction adopts pending rules under one cache_break | `chat/tests/session_compaction.rs::a_compaction_adopts_pending_rules_under_one_cache_break` |
| context | a lost cache_break is announced once by the next attempt | `chat/tests/session_compaction.rs::a_lost_cache_break_is_announced_by_the_next_attempt_once` |
| context | prefix comparison accepts appends and reports rewrites | `testkit/tests/prefix_comparison.rs::accepts_appends_but_reports_rewrites` |
| context | an exception needs a recorded cache_break between the requests | `testkit/tests/prefix_comparison.rs::exception_requires_a_recorded_event_between_requests` |
| memory | the extractor sees only the owner's typed words | `runtime/tests/memory_capture.rs::mem_04_the_extractor_sees_only_the_owners_typed_words` |
| memory | a message the owner did not send is never extracted | `runtime/tests/memory_capture.rs::mem_04_a_message_not_sent_by_the_owner_is_never_extracted` |
| memory | a fooled extractor creates no active record from planted text | `runtime/tests/memory_capture.rs::mem_12_a_fooled_extractor_still_creates_no_active_record_from_planted_text` |
| memory | a proposal is in neither search nor the core block until approved | `testkit/tests/memory_tools.rs::mem_06_a_proposal_is_in_neither_search_nor_the_core_block_until_approved` |
| memory | purge leaves neither text nor id in the database file | `store/tests/memory_sqlite.rs::purge_leaves_neither_text_nor_id_in_the_database_file` |
| memory | purge removes the vector rows with the record | `store/tests/memory_sqlite.rs::purge_removes_the_vector_rows_with_the_record` |
| delegation | a child of a read-only parent has no write tools | `agents/tests/subagents.rs::general_child_of_a_read_only_parent_has_no_write_tools` |
| delegation | children never get memory_propose or memory_forget | `agents/tests/subagents.rs::children_never_get_memory_propose_or_forget` |
| delegation | a workspaceless parent gets read-only children | `agents/tests/subagents.rs::a_workspaceless_chat_parent_gets_read_only_children` |
| delegation | a later role edit never widens a live child | `agents/tests/subagents.rs::widening_a_role_never_widens_a_live_child` |
| delegation | the parked parent frees its slot: pool of two runs three children | `agents/tests/parked_wait.rs::parked_parent_frees_its_slot_so_pool_two_runs_children_in_parallel` |
| delegation | a restart mid-wait resumes the parent without rerunning it | `agents/tests/parked_wait.rs::a_restart_mid_wait_resumes_the_parked_parent_without_rerunning_it` |
| delegation | cancel settles the child and frees its lease | `agents/tests/subagents.rs::cancel_settles_the_child_and_frees_its_lease` |
| delegation | concurrent resumes revive a child exactly once | `agents/tests/subagents.rs::concurrent_resumes_revive_the_child_exactly_once` |
| delegation | restart recovery cancels the children of settled parents | `agents/tests/subagents.rs::restart_recovery_cancels_children_of_settled_parents` |
| accounting | chat usage maps detail counts and omits absent or non-numeric ones | `providers/tests/providers.rs::chat_usage_maps_detail_counts_and_omits_absent_or_non_numeric_ones` |
| accounting | responses usage maps detail counts and omits absent or non-numeric ones | `providers/tests/providers.rs::responses_usage_maps_detail_counts_and_omits_absent_or_non_numeric_ones` |
| accounting | cache-write usage handles unknown, zero and partial splits | `providers/tests/providers.rs::cache_write_usage_handles_unknown_zero_and_partial_splits` |
| accounting | usage observed before a cancel or timeout is kept | `core/tests/run_safety.rs::cancellation_and_timeout_preserve_already_observed_usage_without_waiting_for_cleanup` |
| oracle_integrity | the conformance judge reads final state, not just that a tool was called | `providers/tests/qualification.rs::judge_reads_final_state_not_just_that_a_tool_was_called` |
| oracle_integrity | the judge checks order, unoffered tools and forbidden successes | `providers/tests/qualification.rs::judge_checks_order_unoffered_tools_and_forbidden_successes` |
| oracle_integrity | a model that never calls tools fails on final state | `runtime/tests/tool_use_qualification.rs::a_model_that_never_calls_tools_fails_on_final_state` |

One case has no existing owner and is pinned here instead: the Responses
lane's handling of an *unoffered* tool (see Findings, item 3).

## Findings

Recorded, not fixed (no runtime code was changed).

1. **`context_capture` example tests fail in the current tree.**
   `cargo test -p agentkit-runtime --example context_capture` reports 2
   failures: `context_eval::tests::capture_reaches_mutation_and_report_retains_failures`
   (`runtime/examples/context_eval/tests.rs:57`, `extra_read` never appears in
   any request) and `context_eval::replay_tests::immutable_corpus_reassembles_ten_scripted_sessions`
   (`runtime/examples/context_eval/replay_tests.rs:54`, `request 0 differs`).
   Neither depends on the uncommitted `recorder.rs` change.
2. **A tool contributed mid-session is never offered**
   (`tests/context.rs::a_restaged_tool_is_offered_after_its_contributor_turns_on`,
   `#[ignore = "bug: ..."]`). Across 30 turns on both lanes and both profiles
   the `SwitchableContributor` turned on at turn 8 never shows up in a request
   body. If the frozen tool manifest is the intent, finding 1's example test
   and the report's "Contributor visibility restaged" source hint
   (`runtime/examples/context_eval/report.rs:36`) are stale; if not, restaging
   is broken. Needs the context owner's call.
3. **Responses lane: an unoffered tool aborts the whole response.**
   `providers/src/responses_state.rs:513` (`tool_call`) returns
   `unknown_tool` when the name is not in the request's tools or the namespace
   is not `agentkit`. The run fails (`run.failed`, `errorCode: unknown_tool`);
   no call gets a per-call "unknown tool" result as on Chat Completions
   (`call_unknown` in `cc_tool_calls_mixed.sse`). Pinned by
   `tool_pipeline::responses_refuses_an_unoffered_tool_loudly_and_runs_nothing`;
   confirm it is intended.
4. **The capture harness cannot reach masking or compaction.**
   `runtime/examples/context_eval/capture.rs` fixes the session context to
   `{"policy":"transcript"}` with the default 32768-token window, so a larger
   tool result ends in `context_overflow` (turn 8 at 20 KB per result) before
   any mask or `context.cache_break` appears. Mask and compact breaks are
   covered by `chat/tests/batched_masking.rs` instead (table above).
5. **Contract text vs. code, usage field names.** `docs/contracts.md`
   ("`run.usage`") describes normalized counters as additive snake_case
   (`input_uncached`, `cache_read`, ...), which matches the emitted events;
   `promptTokens`, `completionTokens`, `totalTokens` stay camelCase in the same
   event. Not a bug; noted because suites assert both spellings.
6. **No usage event for a server that reports none.** This matches the
   contract text ("only emits `run.usage` when the server actually reported
   usage"), so "exactly one final `run.usage` per request" holds for
   requests whose server reported usage. The sub-agent token ledger has an
   `unreported` bucket for such a request (`agents/src/subagents/budget.rs`);
   that path was read, not re-tested here.

## Adding an invariant

1. Check the table above first; if a test owns it, add a row to
   `coverage::OWNERS`, regenerate the table with `markdown_table()`.
2. Otherwise add a fixture under `fixtures/http/` (raw bytes, as the server
   would send them) and a test in the matching suite file.
3. A fault is a fixture or an explicit injected failure, never randomness.
4. If the runtime is wrong, do not change it from here: add the test with
   `#[ignore = "bug: ..."]` and a Findings entry.
