//! Production tool-use qualification driver (R21, TU-36).
//!
//! Every run goes through `AgentRuntime::builder`: the real coding toolpack
//! (registered on the coding lane), the real memory tools, the session
//! service, the model's resolved tool profile frozen into the session, the
//! profile's dialect, the guards, the durable run loop and its outcome
//! classifier. Only the effects are mocked, behind their real ports: the
//! workspace is a temp directory, the memory store is the in-memory
//! repository, and the model is scripted unless a live provider is given.
//! Nothing here substitutes a tool schema, a description or a loop.
pub mod live;
pub mod recorder;

use agentkit_contracts::{AiModelToolProfile, AiToolOutcomeClass};
use agentkit_core::{
    ChatProvider, ModelFacts, ProfileQuery, TOOL_CALL, ToolDefinition, ToolLane,
    model_profile_digest, resolve_profile, session_dialect,
};
use agentkit_memory::{MemoryRepository, Page, RecordFilter};
use agentkit_providers::conformance::{
    Observed, ObservedCall, ObservedMemory, QualificationKey, QualificationRecord, Scenario,
    ScenarioResult, Thresholds, Variant, Verdict, digest_json,
};
use agentkit_runtime::AgentRuntime;
use agentkit_sessions::{
    AgentSendResult, CreateSessionInput, SendInput, WhenBusy, with_model_profile,
};
use agentkit_store::{SqliteStore, TaskStatus};
use agentkit_testkit::context_fixtures::recorded_events;
use agentkit_testkit::{InMemoryMemoryRepository, InMemoryRetrievalIndex};
use recorder::{Behaviour, Capture, Recorder, ScriptedModel};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub enum Source {
    Scripted(Behaviour),
    Live(Arc<dyn ChatProvider>),
}

pub struct Target {
    /// Provider kind the profile is matched on.
    pub provider: String,
    pub model: String,
    pub server: String,
    pub source: Source,
    /// Per-scenario wall-clock limit.
    pub timeout: Duration,
}

pub struct RunDetail {
    pub scenario: String,
    pub lang: String,
    pub observed: Observed,
    pub verdict: Verdict,
    pub capture: Capture,
    pub profile: AiModelToolProfile,
}

pub struct Qualification {
    pub record: QualificationRecord,
    pub runs: Vec<RunDetail>,
}

/// The session profile: the built-in coding profile plus the memory tools,
/// which is what a Code host with memory configured offers.
fn session_profile() -> Value {
    json!({
        "id": "qualification/code",
        "extends": "coding",
        "tools": [
            "read_file", "list_dir", "find_files", "code_search", "repo_map", "git_inspect",
            "edit_file", "write_file", "run_command", "run_tests", "submit_plan",
            "memory_search", "memory_get", "memory_propose", "memory_forget"
        ],
        "limits": {"maxToolIterations": 12}
    })
}

fn read_tree(root: &Path, dir: &Path, into: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            read_tree(root, &path, into);
        } else if let (Ok(text), Ok(rel)) =
            (std::fs::read_to_string(&path), path.strip_prefix(root))
        {
            into.insert(rel.to_string_lossy().replace('\\', "/"), text);
        }
    }
}

/// The calls the run loop recorded, in completion order, with its class.
async fn observed_calls(store: &SqliteStore, turn: &str) -> Vec<ObservedCall> {
    let events = recorded_events(store, turn).await.expect("events");
    let mut requested: BTreeMap<String, (String, Value)> = BTreeMap::new();
    let mut calls = Vec::new();
    for record in events {
        let (kind, data) = (&record.event["type"], &record.event["data"]);
        if kind == "run.tool.requested" {
            let args = data["argumentsJson"].as_str().unwrap_or_default();
            let args = serde_json::from_str(args).unwrap_or_else(|_| json!(args));
            let emitted = data["toolName"].as_str().unwrap_or_default().to_owned();
            requested.insert(
                data["toolCallId"].as_str().unwrap_or_default().into(),
                (emitted, args),
            );
        } else if kind == "run.tool.record" {
            let id = data["toolCallId"].as_str().unwrap_or_default();
            let tool = data["canonicalTool"]
                .as_str()
                .or(data["toolName"].as_str())
                .unwrap_or_default();
            let class: AiToolOutcomeClass =
                serde_json::from_value(data["outcomeClass"].clone()).expect("outcome class");
            let (emitted, mut args) = requested.get(id).cloned().unwrap_or_default();
            if emitted == TOOL_CALL {
                // The meta tool wraps the real call: judge the inner arguments.
                args = match args.get("arguments") {
                    Some(Value::String(text)) => serde_json::from_str(text).unwrap_or(Value::Null),
                    Some(inner) => inner.clone(),
                    None => Value::Null,
                };
            }
            calls.push(ObservedCall {
                tool: tool.into(),
                emitted,
                args,
                class,
            });
        }
    }
    calls
}

async fn pump(runtime: &AgentRuntime, store: &SqliteStore, turn: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        runtime.host().run_once().await.expect("pump");
        let task = store.get_task(turn).await.expect("task").expect("exists");
        match task.status {
            TaskStatus::Completed => return true,
            TaskStatus::Failed | TaskStatus::Cancelled => return false,
            _ => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    false
}

async fn memory_of(repository: &InMemoryMemoryRepository) -> Vec<ObservedMemory> {
    let all = repository
        .list(RecordFilter::default(), Page::all())
        .await
        .expect("memory list");
    all.into_iter()
        .map(|record| ObservedMemory {
            status: serde_json::to_value(record.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            text: record.text,
        })
        .collect()
}

pub async fn run_one(target: &Target, scenario: &Scenario, variant: &Variant) -> RunDetail {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("ws");
    for (path, text) in &scenario.files {
        let file = workspace.join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(file, text).expect("seed file");
    }
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SqliteStore::open(dir.path().join("store.sqlite"), 8).expect("store");
    let repository = Arc::new(InMemoryMemoryRepository::new());
    let index = Arc::new(InMemoryRetrievalIndex::new());
    let profile = resolve_profile(&ProfileQuery {
        provider: &target.provider,
        model: &target.model,
        facts: ModelFacts::default(),
        user_profile: None,
    })
    .expect("model profile");
    let inner = match &target.source {
        Source::Scripted(behaviour) => {
            let dialect = session_dialect(None, Some(&profile), ToolLane::ChatCompletions);
            ScriptedModel::new(variant.script.clone(), *behaviour, dialect) as Arc<dyn ChatProvider>
        }
        Source::Live(provider) => Arc::clone(provider),
    };
    let (recorder, capture) = Recorder::new(inner);
    let provider_id = recorder.id().to_owned();
    let mut runtime = AgentRuntime::builder(store.clone())
        .default_provider(recorder)
        .session_defaults(Some(provider_id), Some(target.model.clone()))
        .profiles(vec![session_profile()])
        .memory(repository.clone() as Arc<dyn MemoryRepository>, index)
        .build()
        .await
        .expect("runtime");
    if let Some(worker) = runtime.take_worker() {
        worker.shutdown().await.expect("park worker");
    }
    let root = workspace.display().to_string();
    runtime
        .coding_lane()
        .register(&json!({"workspaceRoot": root, "sandbox": "off"}).to_string())
        .await
        .expect("coding tools");
    runtime
        .service()
        .create_session_with_host_metadata(&CreateSessionInput {
            session_id: Some("s1".into()),
            profile_id: "qualification/code".into(),
            model: Some(target.model.clone()),
            workspace: Some(json!({"root": root, "trusted": true})),
            execution_mode: Some(scenario.execution_mode.clone()),
            metadata: Some(with_model_profile(&json!({}), &profile)),
            ..CreateSessionInput::default()
        })
        .await
        .expect("session");
    let sent = runtime
        .service()
        .send(
            "s1",
            &SendInput {
                frozen_payload: None,
                content: json!(variant.prompt),
                input_id: None,
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .expect("send");
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("the first turn is admitted at once")
    };
    let finished = pump(&runtime, &store, &turn_id, target.timeout).await;
    let calls = observed_calls(&store, &turn_id).await;
    let mut files = BTreeMap::new();
    read_tree(&workspace, &workspace, &mut files);
    runtime.coding_lane().dispose().await;
    let capture = std::mem::take(&mut *capture.lock().expect("capture lock"));
    let observed = Observed {
        offered: capture.tools.iter().map(|t| t.name.clone()).collect(),
        calls,
        files,
        memory: memory_of(&repository).await,
        answer: capture.answer.clone(),
        settled: finished && !capture.answer.is_empty(),
    };
    let verdict = scenario.judge(variant, &observed);
    RunDetail {
        scenario: scenario.id.clone(),
        lang: variant.lang.clone(),
        observed,
        verdict,
        capture,
        profile,
    }
}

fn schema_digest(runs: &[RunDetail]) -> String {
    let mut by_name: BTreeMap<String, Value> = BTreeMap::new();
    for definition in runs.iter().flat_map(|r| &r.capture.tools) {
        by_name.insert(definition.name.clone(), tool_json(definition));
    }
    digest_json(&Value::Array(by_name.into_values().collect()))
}

fn tool_json(definition: &ToolDefinition) -> Value {
    serde_json::to_value(definition).expect("tool definition serializes")
}

fn prompt_digest(runs: &[RunDetail], prompts: &[(String, String, String)]) -> String {
    let system: BTreeSet<&String> = runs.iter().flat_map(|r| &r.capture.system).collect();
    digest_json(&json!({"system": system, "prompts": prompts}))
}

/// Runs every scenario in both languages and builds the record.
pub async fn qualify(
    target: &Target,
    scenarios: &[Scenario],
    thresholds: Thresholds,
    timestamp: String,
    commit: String,
) -> Qualification {
    let mut runs = Vec::new();
    let mut prompts = Vec::new();
    for scenario in scenarios {
        for variant in &scenario.variants {
            prompts.push((
                scenario.id.clone(),
                variant.lang.clone(),
                variant.prompt.clone(),
            ));
            runs.push(run_one(target, scenario, variant).await);
        }
    }
    let profile = runs.first().map(|r| r.profile.clone()).expect("a run");
    let key = QualificationKey {
        model: target.model.clone(),
        provider: target.provider.clone(),
        server: target.server.clone(),
        profile_id: profile.id.clone(),
        profile_version: profile.dialect.version.clone(),
        profile_digest: model_profile_digest(&profile),
        schema_digest: schema_digest(&runs),
        prompt_digest: prompt_digest(&runs, &prompts),
        toolsets: scenarios.iter().map(|s| s.toolset.clone()).collect(),
    };
    let results = runs
        .iter()
        .zip(
            scenarios
                .iter()
                .flat_map(|s| s.variants.iter().map(move |v| (s, v))),
        )
        .map(|(run, (scenario, variant))| {
            ScenarioResult::new(
                &scenario.id,
                &scenario.toolset,
                &variant.lang,
                run.verdict.clone(),
            )
        })
        .collect();
    let record = QualificationRecord::build(key, thresholds, results, timestamp, commit);
    Qualification { record, runs }
}
