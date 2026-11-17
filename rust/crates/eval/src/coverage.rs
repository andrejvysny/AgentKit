//! Invariant to existing-test map.
//!
//! Layer A coordinates tests that already exist instead of copying them. A
//! row here names an invariant and the test that owns it; `tests/coverage.rs`
//! fails when the file or the function is gone, and when the README table no
//! longer says what this table says. Paths are relative to `rust/crates`.
use std::path::PathBuf;

/// One invariant and the test that checks it.
#[derive(Clone, Copy, Debug)]
pub struct Owner {
    pub suite: &'static str,
    pub invariant: &'static str,
    pub path: &'static str,
    pub test: &'static str,
}

const fn owner(
    suite: &'static str,
    invariant: &'static str,
    path: &'static str,
    test: &'static str,
) -> Owner {
    Owner {
        suite,
        invariant,
        path,
        test,
    }
}

/// Tests that own an invariant outside this crate.
pub const OWNERS: &[Owner] = &[
    owner(
        "tool_pipeline",
        "results of one turn come back contiguous and in call order",
        "core/tests/call_pipeline.rs",
        "results_of_one_turn_are_contiguous_and_in_call_order",
    ),
    owner(
        "tool_pipeline",
        "a call without a name still gets a result",
        "core/tests/call_pipeline.rs",
        "unnamed_call_gets_a_result_that_says_the_name_was_missing",
    ),
    owner(
        "tool_pipeline",
        "an unknown tool name gets a teaching failure",
        "core/tests/call_pipeline.rs",
        "unknown_name_lists_three_closest_names_and_points_to_tool_search",
    ),
    owner(
        "tool_pipeline",
        "aliases and text values are normalized and recorded",
        "core/tests/call_pipeline.rs",
        "alias_and_text_values_reach_the_tool_canonical_and_are_recorded",
    ),
    owner(
        "tool_pipeline",
        "one run.tool.record per call, with an outcome class",
        "core/tests/tool_records.rs",
        "every_tool_call_produces_one_record_with_an_outcome_class",
    ),
    owner(
        "tool_pipeline",
        "tool_call meets the same guards as the direct call (allowed)",
        "chat/tests/deferred_tools.rs",
        "ce08_an_allowed_call_runs_identically_through_tool_call",
    ),
    owner(
        "tool_pipeline",
        "tool_call meets the same guards as the direct call (refused)",
        "chat/tests/deferred_tools.rs",
        "ce08_a_refusal_is_the_same_refusal_through_tool_call",
    ),
    owner(
        "tool_pipeline",
        "an approval binds the inner tool and its arguments",
        "chat/tests/deferred_tools.rs",
        "ce08_an_approval_names_the_inner_tool_and_binds_the_inner_arguments",
    ),
    owner(
        "tool_pipeline",
        "a dialect call gets the same guard verdict as the canonical call",
        "runtime/tests/dialect_equivalence.rs",
        "the_translated_call_gets_the_same_guard_verdict_as_the_direct_call",
    ),
    owner(
        "tool_pipeline",
        "a dialect never widens authority",
        "runtime/tests/dialect_equivalence.rs",
        "a_dialect_never_widens_authority",
    ),
    owner(
        "context",
        "sixty tool-heavy turns break the cache only at mask points (cache_break per mask)",
        "chat/tests/batched_masking.rs",
        "sixty_tool_heavy_turns_break_the_cache_only_at_mask_points",
    ),
    owner(
        "context",
        "masked results stay readable through read_output",
        "chat/tests/batched_masking.rs",
        "masked_results_stay_readable_through_their_handle",
    ),
    owner(
        "context",
        "a result quoting another call's handle gets its own output",
        "chat/tests/batched_masking.rs",
        "a_result_quoting_another_calls_handle_gets_its_own_output",
    ),
    owner(
        "context",
        "a compaction adopts pending rules under one cache_break",
        "chat/tests/session_compaction.rs",
        "a_compaction_adopts_pending_rules_under_one_cache_break",
    ),
    owner(
        "context",
        "a lost cache_break is announced once by the next attempt",
        "chat/tests/session_compaction.rs",
        "a_lost_cache_break_is_announced_by_the_next_attempt_once",
    ),
    owner(
        "context",
        "prefix comparison accepts appends and reports rewrites",
        "testkit/tests/prefix_comparison.rs",
        "accepts_appends_but_reports_rewrites",
    ),
    owner(
        "context",
        "an exception needs a recorded cache_break between the requests",
        "testkit/tests/prefix_comparison.rs",
        "exception_requires_a_recorded_event_between_requests",
    ),
    owner(
        "memory",
        "the extractor sees only the owner's typed words",
        "runtime/tests/memory_capture.rs",
        "mem_04_the_extractor_sees_only_the_owners_typed_words",
    ),
    owner(
        "memory",
        "a message the owner did not send is never extracted",
        "runtime/tests/memory_capture.rs",
        "mem_04_a_message_not_sent_by_the_owner_is_never_extracted",
    ),
    owner(
        "memory",
        "a fooled extractor creates no active record from planted text",
        "runtime/tests/memory_capture.rs",
        "mem_12_a_fooled_extractor_still_creates_no_active_record_from_planted_text",
    ),
    owner(
        "memory",
        "a proposal is in neither search nor the core block until approved",
        "testkit/tests/memory_tools.rs",
        "mem_06_a_proposal_is_in_neither_search_nor_the_core_block_until_approved",
    ),
    owner(
        "memory",
        "purge leaves neither text nor id in the database file",
        "store/tests/memory_sqlite.rs",
        "purge_leaves_neither_text_nor_id_in_the_database_file",
    ),
    owner(
        "memory",
        "purge removes the vector rows with the record",
        "store/tests/memory_sqlite.rs",
        "purge_removes_the_vector_rows_with_the_record",
    ),
    owner(
        "delegation",
        "a child of a read-only parent has no write tools",
        "agents/tests/subagents.rs",
        "general_child_of_a_read_only_parent_has_no_write_tools",
    ),
    owner(
        "delegation",
        "children never get memory_propose or memory_forget",
        "agents/tests/subagents.rs",
        "children_never_get_memory_propose_or_forget",
    ),
    owner(
        "delegation",
        "a workspaceless parent gets read-only children",
        "agents/tests/subagents.rs",
        "a_workspaceless_chat_parent_gets_read_only_children",
    ),
    owner(
        "delegation",
        "a later role edit never widens a live child",
        "agents/tests/subagents.rs",
        "widening_a_role_never_widens_a_live_child",
    ),
    owner(
        "delegation",
        "the parked parent frees its slot: pool of two runs three children",
        "agents/tests/parked_wait.rs",
        "parked_parent_frees_its_slot_so_pool_two_runs_children_in_parallel",
    ),
    owner(
        "delegation",
        "a restart mid-wait resumes the parent without rerunning it",
        "agents/tests/parked_wait.rs",
        "a_restart_mid_wait_resumes_the_parked_parent_without_rerunning_it",
    ),
    owner(
        "delegation",
        "cancel settles the child and frees its lease",
        "agents/tests/subagents.rs",
        "cancel_settles_the_child_and_frees_its_lease",
    ),
    owner(
        "delegation",
        "concurrent resumes revive a child exactly once",
        "agents/tests/subagents.rs",
        "concurrent_resumes_revive_the_child_exactly_once",
    ),
    owner(
        "delegation",
        "restart recovery cancels the children of settled parents",
        "agents/tests/subagents.rs",
        "restart_recovery_cancels_children_of_settled_parents",
    ),
    owner(
        "accounting",
        "chat usage maps detail counts and omits absent or non-numeric ones",
        "providers/tests/providers.rs",
        "chat_usage_maps_detail_counts_and_omits_absent_or_non_numeric_ones",
    ),
    owner(
        "accounting",
        "responses usage maps detail counts and omits absent or non-numeric ones",
        "providers/tests/providers.rs",
        "responses_usage_maps_detail_counts_and_omits_absent_or_non_numeric_ones",
    ),
    owner(
        "accounting",
        "cache-write usage handles unknown, zero and partial splits",
        "providers/tests/providers.rs",
        "cache_write_usage_handles_unknown_zero_and_partial_splits",
    ),
    owner(
        "accounting",
        "usage observed before a cancel or timeout is kept",
        "core/tests/run_safety.rs",
        "cancellation_and_timeout_preserve_already_observed_usage_without_waiting_for_cleanup",
    ),
    owner(
        "oracle_integrity",
        "the conformance judge reads final state, not just that a tool was called",
        "providers/tests/qualification.rs",
        "judge_reads_final_state_not_just_that_a_tool_was_called",
    ),
    owner(
        "oracle_integrity",
        "the judge checks order, unoffered tools and forbidden successes",
        "providers/tests/qualification.rs",
        "judge_checks_order_unoffered_tools_and_forbidden_successes",
    ),
    owner(
        "oracle_integrity",
        "a model that never calls tools fails on final state",
        "runtime/tests/tool_use_qualification.rs",
        "a_model_that_never_calls_tools_fails_on_final_state",
    ),
];

/// `rust/crates`, from this crate's manifest.
pub fn crates_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory")
        .to_path_buf()
}

/// `Ok` when the owner's file exists and defines the test function.
pub fn verify(owner: &Owner) -> Result<(), String> {
    let path = crates_dir().join(owner.path);
    let source =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let needle = format!("fn {}(", owner.test);
    if source.contains(&needle) {
        Ok(())
    } else {
        Err(format!("{} no longer defines {}", owner.path, owner.test))
    }
}

/// The README table, one row per owner. The README must contain it verbatim.
pub fn markdown_table() -> String {
    let mut table = String::from("| Suite | Invariant | Owning test |\n|---|---|---|\n");
    for owner in OWNERS {
        table.push_str(&format!(
            "| {} | {} | `{}::{}` |\n",
            owner.suite, owner.invariant, owner.path, owner.test
        ));
    }
    table
}
