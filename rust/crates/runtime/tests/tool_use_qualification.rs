//! R21: qualification through the production pipeline.
//!
//! Default runs use a scripted model so the driver itself is exercised with
//! no network: real coding and memory tools, the model's frozen profile and
//! dialect, the guards and the durable run loop, with a temp-dir workspace
//! and the in-memory memory ports. The live run is opt-in:
//!
//! AGENTKIT_LIVE_CONFORMANCE=1 AGENTKIT_LIVE_BASE_URL=... AGENTKIT_LIVE_MODEL=... \
//!   cargo test -p agentkit-runtime --test tool_use_qualification -- --ignored --nocapture
//!
//! See docs/qualification/tool-use-qualification.md.
mod qualification;

use agentkit_providers::conformance::{Thresholds, production_scenarios};
use qualification::recorder::Behaviour;
use qualification::{Source, Target, qualify};
use std::time::Duration;

fn scripted(behaviour: Behaviour) -> Target {
    Target {
        provider: "openai-compatible".into(),
        model: "qwen3.8-27b".into(),
        server: "scripted".into(),
        source: Source::Scripted(behaviour),
        timeout: Duration::from_secs(30),
    }
}

fn commit() -> String {
    std::env::var("AGENTKIT_COMMIT")
        .ok()
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[tokio::test(flavor = "multi_thread")]
async fn scripted_model_passes_every_scenario_through_the_production_pipeline() {
    let run = qualify(
        &scripted(Behaviour::Follow),
        &production_scenarios(),
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    for detail in &run.runs {
        assert!(
            detail.verdict.passed,
            "{} {}: {:?} / offered {:?} / calls {:?}",
            detail.scenario,
            detail.lang,
            detail.verdict.failures,
            detail.observed.offered,
            detail.observed.calls
        );
    }
    assert_eq!(run.record.scenarios.len(), 12);
    assert_eq!(run.record.qualified_toolsets, ["files", "memory"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_model_is_offered_real_schemas_and_its_own_profile() {
    let run = qualify(
        &scripted(Behaviour::Follow),
        &production_scenarios()[..1],
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    let capture = &run.runs[0].capture;
    let read = capture
        .tools
        .iter()
        .find(|t| t.name == "read_file")
        .expect("read_file is offered");
    assert!(
        read.input_schema["properties"]["file_path"]["type"] == "string"
            && read.input_schema["required"] == serde_json::json!(["file_path"]),
        "{:?}",
        read.input_schema
    );
    assert!(read.input_schema["properties"]["offset"]["type"] == "integer");
    assert!(read.description.len() > 60 && !read.description.starts_with("Conformance tool"));
    // Profile sampling is not overridden: the request carries the frozen
    // profile and no forced temperature.
    assert!(
        capture
            .profile_ids
            .iter()
            .all(|id| id.as_deref() == Some("qwen3.8-27b"))
    );
    assert!(capture.temperatures.iter().all(|t| *t != Some(0.0)));
    assert_eq!(run.record.key.profile_id, "qwen3.8-27b");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_model_that_never_calls_tools_fails_on_final_state() {
    let run = qualify(
        &scripted(Behaviour::NeverCalls),
        &production_scenarios(),
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    let passed: Vec<&str> = run
        .record
        .scenarios
        .iter()
        .filter(|s| s.passed)
        .map(|s| s.scenario.as_str())
        .collect();
    // Only the refusal scenario can pass without a tool call: nothing changed.
    assert!(passed.iter().all(|id| *id == "refusal"), "{passed:?}");
    assert!(run.record.qualified_toolsets.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_record_is_stable_and_valid_only_for_its_exact_tuple() {
    let scenarios = production_scenarios();
    let first = qualify(
        &scripted(Behaviour::Follow),
        &scenarios,
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    let second = qualify(
        &scripted(Behaviour::Follow),
        &scenarios,
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    assert_eq!(
        first.record.key, second.record.key,
        "digests must not depend on temp paths"
    );
    assert!(
        first.record.key.schema_digest.len() == 64 && first.record.key.prompt_digest.len() == 64
    );

    let key = first.record.key.clone();
    assert!(first.record.qualifies(&key, "files") && first.record.qualifies(&key, "memory"));
    let other_model = agentkit_providers::conformance::QualificationKey {
        model: "qwen3.8-27b-other".into(),
        ..key.clone()
    };
    assert!(!first.record.qualifies(&other_model, "files"));

    // Changing a prompt changes the prompt digest, so the old record no longer applies.
    let mut edited = production_scenarios();
    edited[0].variants[0].prompt.push_str(" Be brief.");
    let third = qualify(
        &scripted(Behaviour::Follow),
        &edited,
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    assert_ne!(third.record.key.prompt_digest, key.prompt_digest);
    assert!(!first.record.qualifies(&third.record.key, "files"));
}

/// Live qualification: opt-in, never part of a default run. Needs
/// `AGENTKIT_LIVE_CONFORMANCE=1`, `AGENTKIT_LIVE_BASE_URL` and
/// `AGENTKIT_LIVE_MODEL`; see docs/qualification/tool-use-qualification.md.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs AGENTKIT_LIVE_CONFORMANCE=1 and an endpoint; see docs/qualification/tool-use-qualification.md"]
async fn live_qualification() {
    let env = match qualification::live::LiveEnv::from_env() {
        Ok(Some(env)) => env,
        Ok(None) => {
            eprintln!("skipped: AGENTKIT_LIVE_CONFORMANCE != 1");
            return;
        }
        Err(message) => panic!("{message}"),
    };
    let target = Target {
        provider: env.provider.clone(),
        model: env.model.clone(),
        server: env.server.clone(),
        source: Source::Live(env.provider().expect("live provider")),
        timeout: Duration::from_secs(
            std::env::var("AGENTKIT_LIVE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        ),
    };
    let run = qualify(
        &target,
        &production_scenarios(),
        Thresholds::default(),
        now(),
        commit(),
    )
    .await;
    let out = std::env::var("AGENTKIT_QUALIFICATION_OUT").unwrap_or_else(|_| ".".into());
    std::fs::create_dir_all(&out).expect("output directory");
    let name: String = env
        .model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let path = std::path::Path::new(&out).join(format!("qualification-{name}.json"));
    std::fs::write(&path, run.record.to_json()).expect("write record");
    for result in &run.record.scenarios {
        eprintln!(
            "{} {} {}: {}",
            result.scenario,
            result.lang,
            if result.passed { "pass" } else { "FAIL" },
            result.failures.join("; ")
        );
    }
    eprintln!(
        "qualified toolsets: {:?}; record: {}",
        run.record.qualified_toolsets,
        path.display()
    );
}

/// Live Chat Completions and Responses providers end a turn with a
/// `run.message.completed` event, not `Completed`. The recorder must read the
/// answer from it, or every live scenario reports "did not settle" (F1).
#[tokio::test]
async fn recorder_reads_the_answer_from_a_live_completed_event() {
    use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
    use futures::StreamExt;
    use qualification::recorder::Recorder;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct LiveShaped;
    #[async_trait::async_trait]
    impl ChatProvider for LiveShaped {
        fn id(&self) -> &str {
            "live-shaped"
        }
        fn stream_chat(&self, _input: ChatRequest, _c: CancellationToken) -> ProviderStream {
            Box::pin(futures::stream::iter([Ok(ProviderEvent::Event {
                kind: "run.message.completed".into(),
                data: serde_json::json!({
                    "content": "Heron",
                    "toolCallCount": 0,
                    "toolCalls": [],
                    "finishReason": "stop",
                }),
            })]))
        }
    }

    let (recorder, capture) = Recorder::new(Arc::new(LiveShaped));
    let events: Vec<_> = recorder
        .stream_chat(ChatRequest::default(), CancellationToken::new())
        .collect()
        .await;
    assert_eq!(events.len(), 1);
    assert_eq!(capture.lock().expect("capture lock").answer, "Heron");
}
