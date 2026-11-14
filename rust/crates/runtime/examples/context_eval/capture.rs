use agentkit_chat::{ChatExecutor, ChatExecutorOptions, FileProjectInstructions};
use agentkit_core::{ChatProvider, ToolRegistry};
use agentkit_engine::{AgentHost, HostOptions, SharedRuntime, SystemRuntime, TaskExecutor};
use agentkit_providers::{ChatCompletionsProvider, ResponsesProvider};
use agentkit_sessions::{
    AgentSendResult, AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND,
    CreateSessionInput, ProfileRegistry, SendInput, WhenBusy,
};
use agentkit_store::SqliteStore;
use agentkit_testkit::{
    context_fixtures::{
        RecordingTransport, ScriptedReadTool, SwitchableContributor, recorded_events,
    },
    prefix_harness::EventMark,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    pub protocol: String,
    pub profile: String,
    pub inputs: Vec<String>,
    #[serde(default)]
    pub frames: Vec<String>,
    pub tool_results: Vec<String>,
    #[serde(default)]
    pub mutations: bool,
    #[serde(default)]
    pub schedule: Option<MutationSchedule>,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MutationSchedule {
    pub rules: bool,
    pub selection: bool,
    pub tools: bool,
}

#[derive(Serialize)]
pub struct Capture {
    pub requests: Vec<(Value, EventMark)>,
    pub events: Vec<Vec<Value>>,
    pub statuses: Vec<String>,
}

struct Harness {
    store: SqliteStore,
    transport: Arc<RecordingTransport<SqliteStore>>,
    host: AgentHost<SqliteStore>,
    service: AgentSessionService<SqliteStore>,
    workspace: tempfile::TempDir,
    skill_revision: Value,
    enabled: Arc<AtomicBool>,
}

fn provider(
    protocol: &str,
    transport: Arc<RecordingTransport<SqliteStore>>,
) -> Arc<dyn ChatProvider> {
    match protocol {
        "chat-completions" => Arc::new(ChatCompletionsProvider::new(transport).unwrap()),
        "responses" => Arc::new(ResponsesProvider::new(transport).unwrap()),
        _ => panic!("unsupported capture protocol: {protocol}"),
    }
}

fn executor(
    config: &CaptureConfig,
    transport: Arc<RecordingTransport<SqliteStore>>,
    enabled: Arc<AtomicBool>,
) -> Arc<dyn TaskExecutor<SqliteStore>> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(ScriptedReadTool::new(
            "read_file",
            "read",
            config.tool_results.clone(),
        )))
        .unwrap();
    let extra = Arc::new(ScriptedReadTool::new("extra_read", "extra tool", vec![]));
    Arc::new(
        ChatExecutor::new(provider(&config.protocol, transport), Arc::new(registry)).with_options(
            ChatExecutorOptions {
                project_instructions: Some(Arc::new(FileProjectInstructions)),
                tool_contributors: vec![Arc::new(SwitchableContributor::new(enabled, extra))],
                ..Default::default()
            },
        ),
    )
}

fn profiles() -> Arc<ProfileRegistry> {
    Arc::new(ProfileRegistry::new(&[
        json!({"id":"chat","instructions":"Chat instructions","limits":{"maxToolIterations":4},"tools":["read_file","extra_read"],"resources":{"projectInstructions":true}}),
        json!({"id":"coding","instructions":"Code instructions","tools":["read_file","extra_read"],"resources":{"projectInstructions":true}}),
    ]).unwrap())
}

async fn skill_revision(store: &SqliteStore) -> Value {
    let skill = store
        .publish_skill(
            "docs".into(),
            "guide".into(),
            "---\nname: guide\ndescription: guide\n---\nselected managed instruction\n".into(),
        )
        .await
        .unwrap();
    skill["revision"].clone()
}

impl Harness {
    async fn new(config: &CaptureConfig) -> Self {
        let store = SqliteStore::memory(64).unwrap();
        let protocol = match config.protocol.as_str() {
            "chat-completions" => "chat-completions",
            "responses" => "responses",
            other => panic!("unsupported capture protocol: {other}"),
        };
        let transport = Arc::new(RecordingTransport::new(
            protocol,
            config.frames.clone(),
            store.clone(),
        ));
        let enabled = Arc::new(AtomicBool::new(false));
        let runtime: SharedRuntime = Arc::new(SystemRuntime);
        let host = AgentHost::new(
            store.clone(),
            HashMap::from([(
                CHAT_TURN_TASK_KIND.to_owned(),
                executor(config, transport.clone(), enabled.clone()),
            )]),
            runtime.clone(),
            HostOptions::default(),
        )
        .unwrap();
        let service = AgentSessionService::new(AgentSessionServiceDeps {
            store: store.clone(),
            runtime,
            plan_approval: None,
            wake: Some(host.wake_handle()),
        })
        .with_profiles(profiles());
        host.set_session_flush_planner(Arc::new(service.clone()))
            .await;
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "initial project rule\n").unwrap();
        let skill_revision = skill_revision(&store).await;
        Self {
            store,
            transport,
            host,
            service,
            workspace,
            skill_revision,
            enabled,
        }
    }

    async fn create_session(&self, profile: &str) {
        self.service
            .create_session(&CreateSessionInput {
                session_id: Some("s1".into()),
                title: None,
                profile_id: profile.into(),
                profile_digest: None,
                reasoning_effort: None,
                content_selection: None,
                provider_id: None,
                model: Some("model".into()),
                workspace: Some(
                    json!({"root":self.workspace.path().display().to_string(),"trusted":true}),
                ),
                context: Some(json!({"toolResultElision":{}})),
                skills: None,
                turn_kind: None,
                execution_mode: None,
                metadata: None,
            })
            .await
            .unwrap();
    }

    async fn mutate(&self, turn: usize, schedule: MutationSchedule) {
        if schedule.rules && turn == 4 {
            std::fs::write(
                self.workspace.path().join("AGENTS.md"),
                "new project rule\n",
            )
            .unwrap();
        }
        if schedule.selection && turn == 6 {
            self.service
                .configure(
                    "s1",
                    Some(vec![
                        json!({"id":"docs:guide","revision":self.skill_revision}),
                    ]),
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        if schedule.tools && turn == 8 {
            self.enabled.store(true, Ordering::SeqCst);
        }
    }

    async fn execute(&self, index: usize, input: &str) -> (String, Vec<Value>) {
        let sent = self
            .service
            .send(
                "s1",
                &SendInput {
                    frozen_payload: None,
                    content: json!(input),
                    input_id: None,
                    when_busy: WhenBusy::Reject,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
            .unwrap();
        let AgentSendResult::Turn { turn_id, .. } = sent else {
            panic!("capture turn queued")
        };
        self.transport.set_current(index, &turn_id);
        self.host.run_once().await.unwrap();
        wait_for_terminal(&self.store, &turn_id).await;
        let task = self.store.get_task(&turn_id).await.unwrap().unwrap();
        let events = recorded_events(&self.store, &turn_id)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event)
            .collect();
        (format!("{:?}", task.status), events)
    }
}

async fn wait_for_terminal(store: &SqliteStore, task: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if store
                .get_task(task)
                .await
                .unwrap()
                .unwrap()
                .status
                .is_terminal()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("capture turn deadline");
}

pub async fn run(config: CaptureConfig) -> Capture {
    let schedule = config.schedule.unwrap_or(MutationSchedule {
        rules: config.mutations,
        selection: config.mutations,
        tools: config.mutations,
    });
    let harness = Harness::new(&config).await;
    harness.create_session(&config.profile).await;
    let mut events = vec![];
    let mut statuses = vec![];
    for (index, input) in config.inputs.iter().enumerate() {
        harness.mutate(index, schedule).await;
        let (status, turn_events) = harness.execute(index, input).await;
        statuses.push(status);
        events.push(turn_events);
    }
    Capture {
        requests: harness.transport.requests(),
        events,
        statuses,
    }
}
