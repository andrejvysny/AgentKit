use super::{capture::run, report, scenarios};
use agentkit_testkit::prefix_harness::{EventMark, compare_recorded};
use serde_json::json;

#[test]
fn scenarios_cover_lanes_profiles_and_mutations() {
    let cases = scenarios::baseline_scenarios();
    assert_eq!(cases.len(), 12);
    assert!(cases.iter().all(|case| case.config.inputs.len() == 30));
    assert_eq!(
        cases.iter().filter(|case| case.name == "control").count(),
        4
    );
}

#[test]
fn recorded_exceptions_and_argument_bytes_are_checked() {
    let first = json!({"messages":[{"arguments":"{ \"x\": 1 }"}]});
    let second = json!({"messages":[{"arguments":"{\"x\":1}"}]});
    let a = EventMark { turn: 0, seq: 0.0 };
    let b = EventMark { turn: 0, seq: 2.0 };
    assert!(compare_recorded(&first, &second, "messages", a, b, &[]).is_some());
    assert!(
        compare_recorded(
            &first,
            &second,
            "messages",
            a,
            b,
            &[EventMark { turn: 0, seq: 1.0 }]
        )
        .is_none()
    );
}

#[tokio::test]
async fn capture_reaches_mutation_and_report_retains_failures() {
    let mut case = scenarios::baseline_scenarios()
        .into_iter()
        .find(|case| case.name == "combined" && case.config.protocol == "chat-completions")
        .unwrap();
    case.config.inputs.truncate(10);
    let capture = run(case.config.clone()).await;
    assert_eq!(capture.statuses.len(), 10);
    assert!(
        capture
            .requests
            .iter()
            .any(|(body, _)| body.to_string().contains("new project rule"))
    );
    assert!(
        capture
            .requests
            .iter()
            .any(|(body, _)| body.to_string().contains("selected managed instruction"))
    );
    assert!(
        capture
            .requests
            .iter()
            .any(|(body, _)| body.to_string().contains("extra_read"))
    );
    let summary = report::summarize(&case, &capture, true);
    assert_eq!(summary["turnCount"], 10);
    assert_eq!(summary["requestCount"], capture.requests.len());
    assert!(summary["rawCapture"].is_object());
}

#[test]
fn report_preserves_failures_and_only_applies_recorded_exceptions() {
    use super::capture::Capture;
    let case = scenarios::baseline_scenarios().into_iter().next().unwrap();
    let capture = Capture {
        requests: vec![
            (
                json!({"messages":[{"text":"old"}]}),
                EventMark { turn: 0, seq: 0.0 },
            ),
            (
                json!({"messages":[{"text":"new"}]}),
                EventMark { turn: 0, seq: 2.0 },
            ),
        ],
        events: vec![vec![
            json!({"type":"context.cache_break","seq":1}),
            json!({"type":"run.failed","seq":3,"data":{"errorCode":"continuation_history_mismatch"}}),
        ]],
        statuses: vec!["Failed".into()],
    };
    let summary = report::summarize(&case, &capture, false);
    assert!(summary["breaks"].as_array().unwrap().is_empty());
    assert_eq!(summary["exceptions"].as_array().unwrap().len(), 1);
    assert_eq!(
        summary["failures"][0]["events"][1]["data"]["errorCode"],
        "continuation_history_mismatch"
    );
    assert!(
        summary["failures"][0]["sourceHint"]
            .as_str()
            .unwrap()
            .contains("responses_request.rs")
    );
    assert!(summary.get("rawCapture").is_none());
}

#[tokio::test]
async fn captured_requests_report_their_system_layout() {
    let mut case = scenarios::baseline_scenarios()
        .into_iter()
        .find(|case| case.name == "control" && case.config.protocol == "chat-completions")
        .unwrap();
    case.config.inputs.truncate(4);
    let capture = run(case.config.clone()).await;
    let summary = report::summarize(&case, &capture, false);
    let layout = summary["layout"].as_array().expect("layout report");
    assert_eq!(layout.len(), capture.requests.len());
    assert!(layout.iter().all(|row| row["violations"] == json!([])));
}
