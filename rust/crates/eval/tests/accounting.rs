//! Invariant suite 5: accounting. Each provider request settles exactly one
//! final `run.usage`, and a number the provider did not report stays absent.
//!
//! Bytes are recorded SSE fixtures through the real parsers and the real run
//! loop; the usage events asserted on are the ones the loop emitted.

use agentkit_core::{ChatProvider, Runtime, ToolRegistry};
use agentkit_eval::http_fixture::{FixtureTransport, Protocol};
use agentkit_eval::run::{RecordingTool, Run, drive, options};
use agentkit_providers::{ChatCompletionsProvider, ResponsesProvider, TrustedTransport};
use agentkit_testkit::ManualClock;
use serde_json::{Value, json};
use std::sync::Arc;

async fn run_fixtures(protocol: Protocol, names: &[&str]) -> (Run, Arc<FixtureTransport>) {
    let transport = Arc::new(FixtureTransport::from_fixtures(protocol, names));
    let (tool, _) = RecordingTool::new(
        "read_file",
        "read",
        json!({"type":"object","properties":{"file_path":{"type":"string"},"limit":{"type":"integer"}}}),
    );
    let mut registry = ToolRegistry::new();
    registry.register(tool).expect("register");
    let registry = Arc::new(registry);
    let shared: Arc<dyn TrustedTransport> = transport.clone();
    let provider: Arc<dyn ChatProvider> = match protocol {
        Protocol::ChatCompletions => Arc::new(ChatCompletionsProvider::new(shared).unwrap()),
        Protocol::Responses => Arc::new(ResponsesProvider::new(shared).unwrap()),
    };
    let clock: Arc<dyn Runtime> = ManualClock::new(1_000);
    let run = drive(options(&registry, clock), provider, registry).await;
    (run, transport)
}

const BOTH: [(Protocol, &str, &str); 2] = [
    (
        Protocol::ChatCompletions,
        "cc_tool_calls_mixed.sse",
        "cc_answer.sse",
    ),
    (
        Protocol::Responses,
        "responses_tool_calls_mixed.sse",
        "responses_answer.sse",
    ),
];

#[tokio::test(flavor = "current_thread")]
async fn each_request_settles_exactly_one_final_usage() {
    for (protocol, first, second) in BOTH {
        let (run, transport) = run_fixtures(protocol, &[first, second]).await;
        assert!(run.types().contains(&"run.completed"), "{protocol:?}");
        assert_eq!(transport.requests().len(), 2, "{protocol:?}");

        let usage = run.data("run.usage");
        let finals: Vec<&&Value> = usage
            .iter()
            .filter(|data| data["finalForCall"] == true)
            .collect();
        assert_eq!(finals.len(), 2, "{protocol:?}: one final usage per request");
        assert_eq!(usage.len(), 2, "{protocol:?}: and nothing else");
        assert_eq!(
            finals
                .iter()
                .map(|data| data["step"].clone())
                .collect::<Vec<_>>(),
            [json!(1), json!(2)],
            "{protocol:?}: stamped with the loop iteration"
        );
        assert_ne!(
            finals[0]["callId"], finals[1]["callId"],
            "{protocol:?}: distinct calls have distinct ids"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn reported_counts_normalize_and_unreported_ones_stay_missing() {
    for (protocol, first, second) in BOTH {
        let (run, _) = run_fixtures(protocol, &[first, second]).await;
        let usage = run.data("run.usage");
        // Request 1 reported prompt 120 (100 cached), completion 40 (10 reasoning).
        let one = usage[0];
        assert_eq!(one["cache_read"], 100, "{protocol:?}: {one}");
        assert_eq!(one["reasoning"], 10, "{protocol:?}: {one}");
        assert_eq!(one["output"], 30, "{protocol:?}: output excludes reasoning");
        // Uncached input needs the cache-write count too; it was not
        // reported, so it is unknown rather than 120 - 100.
        assert!(one.get("input_uncached").is_none(), "{protocol:?}: {one}");
        // Request 2 reported prompt and completion only: nothing about the
        // cache or reasoning was said, so nothing is invented.
        let two = usage[1];
        assert_eq!(two["promptTokens"], 200, "{protocol:?}: {two}");
        for key in [
            "cache_read",
            "input_uncached",
            "reasoning",
            "output",
            "cache_write_short",
            "cache_write_long",
        ] {
            assert!(
                two.get(key).is_none(),
                "{protocol:?}: {key} fabricated in {two}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_server_that_reports_no_usage_gets_no_usage_event() {
    for (protocol, answer) in [
        (Protocol::ChatCompletions, "cc_answer_no_usage.sse"),
        (Protocol::Responses, "responses_answer_no_usage.sse"),
    ] {
        let (run, _) = run_fixtures(protocol, &[answer]).await;
        assert!(run.types().contains(&"run.completed"), "{protocol:?}");
        assert!(
            run.data("run.usage").is_empty(),
            "{protocol:?}: usage invented for a server that sent none: {:?}",
            run.data("run.usage")
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_partial_usage_keeps_the_missing_counters_missing() {
    for (protocol, answer) in [
        (Protocol::ChatCompletions, "cc_answer_prompt_only_usage.sse"),
        (
            Protocol::Responses,
            "responses_answer_prompt_only_usage.sse",
        ),
    ] {
        let (run, _) = run_fixtures(protocol, &[answer]).await;
        let usage = run.data("run.usage");
        assert_eq!(usage.len(), 1, "{protocol:?}");
        let data = usage[0];
        assert_eq!(data["finalForCall"], true);
        assert_eq!(data["promptTokens"], 50);
        for key in [
            "completionTokens",
            "totalTokens",
            "cache_read",
            "input_uncached",
            "output",
            "reasoning",
        ] {
            assert!(
                data.get(key).is_none(),
                "{protocol:?}: {key} fabricated in {data}"
            );
        }
    }
}
