//! Invariant suite 2: context. The frozen prefix stays byte-stable across a
//! scripted session, every rewrite announces itself with `context.cache_break`
//! and masked results stay readable.
//!
//! The 30-turn capture harness is the runtime's own (`context_capture`
//! example): it drives the real chat executor, session service, providers and
//! store with scripted transport frames, and records every request body. It
//! is compiled in here with `#[path]` instead of being copied, so there is one
//! harness and one set of prefix assertions (`agentkit_testkit::prefix_harness`).
//! The masked-result and compaction cases already have owners; see
//! `README.md` ("batched_masking.rs", "session_compaction.rs").

#[allow(dead_code)] // the example's report mode uses items these tests do not
#[path = "../../runtime/examples/context_eval/capture.rs"]
mod capture;
#[allow(dead_code)]
#[path = "../../runtime/examples/context_eval/report.rs"]
mod report;
#[allow(dead_code)]
#[path = "../../runtime/examples/context_eval/scenarios.rs"]
mod scenarios;

use capture::run;
use scenarios::{Scenario, baseline_scenarios};
use serde_json::Value;

async fn summarized(case: &Scenario) -> Value {
    let capture = run(case.config.clone()).await;
    report::summarize(case, &capture, false)
}

fn label(case: &Scenario) -> String {
    format!(
        "{}/{}/{}",
        case.config.protocol, case.config.profile, case.name
    )
}

fn count(summary: &Value, key: &str) -> usize {
    summary[key].as_array().map_or(0, Vec::len)
}

#[tokio::test(flavor = "current_thread")]
async fn the_prefix_never_breaks_across_thirty_control_turns() {
    for case in baseline_scenarios().iter().filter(|c| c.name == "control") {
        let summary = summarized(case).await;
        let name = label(case);
        assert_eq!(summary["turnCount"], 30, "{name}");
        assert!(
            summary["statuses"]
                .as_array()
                .unwrap()
                .iter()
                .all(|status| status == "Completed"),
            "{name}: {}",
            summary["failures"]
        );
        assert_eq!(
            count(&summary, "breaks"),
            0,
            "{name}: {}",
            summary["breaks"]
        );
        assert!(
            summary["layout"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["violations"] == serde_json::json!([])),
            "{name}: a system block follows history"
        );
    }
}

/// Rule reload (turn 4) and a managed selection (turn 6) change what the
/// model is told. The session's frozen prefix is only rewritten at a
/// compaction (`session_compaction.rs`), so the changes arrive without a
/// break; a break with no recorded `context.cache_break` mark would be listed
/// under `breaks`.
#[tokio::test(flavor = "current_thread")]
async fn rule_and_selection_changes_reach_the_model_without_an_unannounced_break() {
    for case in baseline_scenarios().iter().filter(|c| c.name == "combined") {
        let capture = run(case.config.clone()).await;
        let summary = report::summarize(case, &capture, false);
        let name = label(case);
        assert_eq!(
            count(&summary, "breaks"),
            0,
            "{name}: {}",
            summary["breaks"]
        );
        assert!(
            summary["statuses"]
                .as_array()
                .unwrap()
                .iter()
                .all(|status| status == "Completed"),
            "{name}: {}",
            summary["failures"]
        );
        let sent: Vec<String> = capture
            .requests
            .iter()
            .map(|(body, _)| body.to_string())
            .collect();
        for needle in ["new project rule", "selected managed instruction"] {
            assert!(
                sent.iter().any(|body| body.contains(needle)),
                "{name}: {needle:?} never reached a request"
            );
        }
        // The announcement and the break agree: every excused pair has a mark.
        assert!(
            count(&summary, "exceptions") <= count(&summary, "cacheBreakEvents"),
            "{name}: {summary}"
        );
    }
}

/// The capture example's own test (`context_eval/tests.rs`,
/// `capture_reaches_mutation_and_report_retains_failures`) expects the tool a
/// contributor switches on at turn 8 to be offered afterwards. In the current
/// tree it never is, on either lane or profile, and that example test fails.
/// Either the frozen tool manifest is intended (then the example test and the
/// report's tools source hint are stale) or restaging is broken; the owner of
/// the context design decides. See README, Findings.
#[tokio::test(flavor = "current_thread")]
#[ignore = "bug: a contributor switched on mid-session never reaches a request (context_capture example test fails the same way)"]
async fn a_restaged_tool_is_offered_after_its_contributor_turns_on() {
    for case in baseline_scenarios().iter().filter(|c| c.name == "combined") {
        let capture = run(case.config.clone()).await;
        assert!(
            capture
                .requests
                .iter()
                .any(|(body, _)| body.to_string().contains("extra_read")),
            "{}: extra_read never offered",
            label(case)
        );
    }
}
