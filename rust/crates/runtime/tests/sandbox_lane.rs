//! The sandbox lane through the real runtime (ADR 0038, WP-1.7): a scripted
//! model runs a command, writes and reads a file, and parks on an export
//! approval (the guard is wired), which resumes once and hands the approved
//! bytes to the host's sink. A runtime without the lane offers no sandbox
//! tools, and every code the pack can answer has a failure-table row.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_core::{CancellationToken, ChatProvider, ChatRequest, ProviderEvent, ProviderStream};
use agentkit_engine::SystemRuntime;
use agentkit_governance::{GovernanceResult, ToolContributionContext};
use agentkit_runtime::{AgentRuntime, SandboxLane};
use agentkit_sessions::{AgentSendResult, CreateSessionInput, SendInput, WhenBusy};
use agentkit_store::{AgentInputKind, AgentInputQuery, AgentInputStatus, SqliteStore, TaskStatus};
use agentkit_testkit::{FakeSandboxBackend, InMemoryWorkspaceRepository, Program};
use agentkit_toolpacks_sandbox::{
    ExportContext, ExportKind, ExportReceipt, ExportSinkError, SandboxBinding,
    SandboxBindingResolver, SandboxEnvironment, SandboxExportSink, SandboxMode,
};
use agentkit_workspaces::{
    AgentId, Binding, ExportFile, FileMode, FileRef, Generation, LeaseHolder, OpContext,
    OperationId, RelPath, RepositoryError, SandboxError, ServiceConfig, Sha256, WorkspaceId,
    WorkspaceKey, WorkspaceKind, WorkspaceService, WriteRequest,
};
use async_trait::async_trait;
use serde_json::{Value, json};

const SESSION: &str = "s1";

/// The agent home every run is pinned to, in Ask before changes.
fn home() -> SandboxBinding {
    SandboxBinding {
        binding: Binding {
            key: WorkspaceKey::new("agent:a1"),
            kind: WorkspaceKind::AgentHome,
            owner_scope: "owner:1".into(),
            folder: None,
        },
        agent_id: AgentId::new("a1"),
        mode: SandboxMode::AskBeforeChanges,
    }
}

struct HomeResolver;

#[async_trait]
impl SandboxBindingResolver for HomeResolver {
    async fn binding(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Option<SandboxBinding>> {
        Ok(Some(home()))
    }
}

#[derive(Default)]
struct RecordingSink {
    exports: Mutex<Vec<(ExportContext, ExportFile, String)>>,
}

#[async_trait]
impl SandboxExportSink for RecordingSink {
    async fn export(
        &self,
        context: &ExportContext,
        file: ExportFile,
        title: &str,
    ) -> Result<ExportReceipt, ExportSinkError> {
        let mut exports = self.exports.lock().unwrap();
        exports.push((context.clone(), file, title.to_owned()));
        Ok(ExportReceipt {
            file_id: format!("file-{}", exports.len()),
            kind: ExportKind::File,
        })
    }
}

/// One step per model request, each one tool call; then `done`.
struct Steps {
    requests: Mutex<Vec<ChatRequest>>,
    steps: Vec<(&'static str, &'static str, Value)>,
    count: AtomicUsize,
}

impl Steps {
    fn new(steps: Vec<(&'static str, &'static str, Value)>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            steps,
            count: AtomicUsize::new(0),
        })
    }
}

impl ChatProvider for Steps {
    fn id(&self) -> &str {
        "test"
    }

    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.requests.lock().unwrap().push(input);
        let n = self.count.fetch_add(1, Ordering::SeqCst);
        let calls: Vec<agentkit_core::ToolCall> = self
            .steps
            .get(n)
            .map(|(id, name, args)| agentkit_core::ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                arguments_json: args.to_string(),
            })
            .into_iter()
            .collect();
        let finish = if calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if calls.is_empty() { "done" } else { "" }.into(),
            finish_reason: finish.into(),
            tool_calls: calls,
        })]))
    }
}

struct World {
    runtime: AgentRuntime,
    store: SqliteStore,
    provider: Arc<Steps>,
    fake: FakeSandboxBackend,
    sink: Arc<RecordingSink>,
}

fn lane(fake: &FakeSandboxBackend, sink: &Arc<RecordingSink>) -> SandboxLane {
    SandboxLane {
        backend: Arc::new(fake.clone()),
        repository: Arc::new(InMemoryWorkspaceRepository::new()),
        config: ServiceConfig {
            poll_interval_ms: 5,
            ..ServiceConfig::new("sha256:image-1")
        },
        resolver: Arc::new(HomeResolver),
        export_sink: sink.clone(),
        authority: None,
        environment: SandboxEnvironment::new("2026.11 (sha256:image-1)"),
    }
}

async fn world(steps: Vec<(&'static str, &'static str, Value)>, with_lane: bool) -> World {
    let store = SqliteStore::memory(64).expect("store");
    let provider = Steps::new(steps);
    let fake = FakeSandboxBackend::new();
    fake.script("echo hi", Program::exit(0).stdout("hi\n"));
    let sink = Arc::new(RecordingSink::default());
    let mut builder = AgentRuntime::builder(store.clone())
        .default_provider(provider.clone())
        .profiles(vec![json!({
            "id": "sandboxed",
            "extends": "coding",
            "tools": ["toolset:sandbox"],
        })]);
    if with_lane {
        builder = builder.sandbox_lane(lane(&fake, &sink));
    }
    let mut runtime = builder.build().await.expect("build");
    // Deterministic driving: only `run_once` below executes the turn.
    if let Some(worker) = runtime.take_worker() {
        worker.shutdown().await.expect("shutdown worker");
    }
    runtime
        .service()
        .create_session(&CreateSessionInput {
            session_id: Some(SESSION.into()),
            profile_id: "sandboxed".into(),
            model: Some("model".into()),
            ..Default::default()
        })
        .await
        .expect("session");
    World {
        runtime,
        store,
        provider,
        fake,
        sink,
    }
}

impl World {
    async fn start_turn(&self) -> String {
        let input = SendInput {
            content: json!("work in the sandbox"),
            input_id: None,
            when_busy: WhenBusy::Reject,
            turn_id: None,
            on_settled: None,
            frozen_payload: None,
        };
        match self.runtime.service().send(SESSION, &input).await.unwrap() {
            AgentSendResult::Turn { turn_id, .. } => turn_id,
            AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
        }
    }

    async fn status(&self, turn_id: &str) -> TaskStatus {
        self.store.get_task(turn_id).await.unwrap().unwrap().status
    }

    async fn pending(&self, turn_id: &str) -> Vec<agentkit_store::AgentInputRecord> {
        self.store
            .list_inputs(AgentInputQuery {
                session_id: SESSION.into(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(turn_id.to_owned()),
            })
            .await
            .unwrap()
    }

    /// Every tool message the model was shown, in order, from the last request.
    fn tool_messages(&self) -> Vec<String> {
        let requests = self.provider.requests.lock().unwrap();
        requests
            .last()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .filter(|m| m.role == "tool")
                    // The envelope as stored: a JSON text.
                    .map(|m| match &m.content {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The model read the command's receipt, the write and the file.
fn assert_run_write_read_shown(shown: &str) {
    assert!(shown.contains("hi\\n[exit code 0]"), "{shown}");
    assert!(
        shown.contains("Wrote 5 bytes to /workspace/a.txt."),
        "{shown}"
    );
    assert!(
        shown.contains("hello"),
        "the read shows the content: {shown}"
    );
}

fn script() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("call-run", "sandbox_run", json!({"command": "echo hi"})),
        (
            "call-write",
            "sandbox_write_file",
            json!({"file_path": "a.txt", "content": "hello"}),
        ),
        (
            "call-read",
            "sandbox_read_file",
            json!({"file_path": "a.txt"}),
        ),
        (
            "call-export",
            "sandbox_export_file",
            json!({"file_path": "a.txt"}),
        ),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scripted_run_executes_parks_on_the_export_and_resumes_once() {
    let w = world(script(), true).await;
    // No home yet: the turn's first command creates it, and the export's
    // approval, asked in that same attempt, must still hold on resume (the
    // effect context binds the attachment, not the machine; ADR 0038
    // decision 14).
    let service = w.runtime.sandbox_service().expect("the lane is exposed");
    assert!(service.lookup(&home().binding).await.unwrap().is_none());
    let turn_id = w.start_turn().await;

    // Run, write and read execute; the export parks on the guard's ask.
    w.runtime.host().run_once().await.unwrap();
    assert_eq!(w.status(&turn_id).await, TaskStatus::WaitingApproval);
    let pending = w.pending(&turn_id).await;
    assert_eq!(pending.len(), 1, "exactly one request awaits the answer");
    assert_eq!(pending[0].content["toolName"], "sandbox_export_file");
    let card = pending[0].content["prompt"].as_str().unwrap_or_default();
    assert!(card.contains("Path: /workspace/a.txt"), "{card}");
    assert_eq!(w.sink.exports.lock().unwrap().len(), 0, "nothing left yet");
    assert_eq!(w.fake.dispatch_calls_total(), 2, "run and write dispatched");
    assert_run_write_read_shown(&w.tool_messages().join("\n"));

    // The owner approves; the export runs once with the approved bytes.
    w.runtime
        .service()
        .approve(&pending[0].input_id, "ans-export", None, None)
        .await
        .unwrap();
    w.runtime.host().run_once().await.unwrap();
    assert_eq!(w.status(&turn_id).await, TaskStatus::Completed);
    let shown = w.tool_messages().join("\n");
    let exports = w.sink.exports.lock().unwrap();
    assert_eq!(exports.len(), 1, "exported exactly once: {shown}");
    assert_eq!(exports[0].1.bytes, b"hello");
    assert_eq!(exports[0].0.path, "/workspace/a.txt");
    assert_eq!(exports[0].2, "a.txt");
    assert_eq!(
        w.fake.dispatch_calls_total(),
        2,
        "the resume launched nothing again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_lane_no_sandbox_tool_is_offered() {
    let w = world(vec![], false).await;
    assert!(w.runtime.sandbox_service().is_none());
    let turn_id = w.start_turn().await;
    w.runtime.host().run_once().await.unwrap();
    assert_eq!(w.status(&turn_id).await, TaskStatus::Completed);
    let requests = w.provider.requests.lock().unwrap();
    assert!(!requests.is_empty(), "the model was asked");
    for request in requests.iter() {
        assert!(
            request
                .tools
                .iter()
                .all(|t| !t.name.starts_with("sandbox_")),
            "no sandbox tool in the tools array"
        );
        let text = format!("{:?}", request.messages);
        assert!(
            !text.contains("sandbox_run"),
            "no sandbox tool in the catalog"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn with_the_lane_the_sandbox_tools_are_offered() {
    let w = world(vec![], true).await;
    let turn_id = w.start_turn().await;
    w.runtime.host().run_once().await.unwrap();
    assert_eq!(w.status(&turn_id).await, TaskStatus::Completed);
    let requests = w.provider.requests.lock().unwrap();
    let request = format!("{:?} {:?}", requests[0].tools, requests[0].messages);
    for name in agentkit_toolpacks_sandbox::SANDBOX_TOOL_NAMES {
        assert!(request.contains(name), "{name} is offered");
    }
}

#[tokio::test]
async fn an_invalid_service_config_is_refused_at_build() {
    let fake = FakeSandboxBackend::new();
    let sink = Arc::new(RecordingSink::default());
    let mut lane = lane(&fake, &sink);
    lane.config.poll_interval_ms = 0;
    let built = AgentRuntime::builder(SqliteStore::memory(8).unwrap())
        .default_provider(Steps::new(vec![]))
        .sandbox_lane(lane)
        .build()
        .await;
    let error = built.err().expect("refused").to_string();
    assert!(error.contains("sandbox lane"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn build_runs_the_sandbox_startup_recovery() {
    let fake = FakeSandboxBackend::new();
    let repo = Arc::new(InMemoryWorkspaceRepository::new());
    // The previous life: a write started a machine of the home's generation.
    let before = WorkspaceService::new(
        repo.clone(),
        Arc::new(fake.clone()),
        Arc::new(SystemRuntime),
        ServiceConfig::new("sha256:image-1"),
    )
    .unwrap();
    let op = OpContext::new(
        OperationId::new("put-1"),
        Sha256::of(b"x"),
        "owner-run",
        AgentId::new("a1"),
    );
    let request = WriteRequest {
        file: FileRef::workspace(RelPath::parse("a.txt").unwrap()),
        bytes: b"x".to_vec(),
        mode: FileMode::REGULAR,
    };
    before
        .write_file(&op, &home().binding, request)
        .await
        .unwrap();
    let old = before.lookup(&home().binding).await.unwrap().unwrap();

    let mut lane = lane(&fake, &Arc::new(RecordingSink::default()));
    lane.repository = repo;
    let mut runtime = AgentRuntime::builder(SqliteStore::memory(8).unwrap())
        .default_provider(Steps::new(vec![]))
        .sandbox_lane(lane)
        .build()
        .await
        .expect("build");
    let service = runtime.sandbox_service().expect("the lane is exposed");
    let now = service.lookup(&home().binding).await.unwrap().unwrap();
    assert!(
        now.generation > old.generation,
        "the leftover machine was proven stopped and its generation advanced"
    );
    if let Some(worker) = runtime.take_worker() {
        worker.shutdown().await.expect("shutdown worker");
    }
}

/// One error of each kind the workspace service answers.
fn service_errors() -> Vec<SandboxError> {
    let holder = LeaseHolder {
        agent_id: AgentId::new("a1"),
        run_id: "r1".into(),
        label: "Researcher".into(),
        host_id: "h".into(),
        boot_id: "b".into(),
    };
    let reason = || "x".to_owned();
    vec![
        SandboxError::NotAttached,
        SandboxError::Unavailable { reason: reason() },
        SandboxError::Busy { holder },
        SandboxError::Fenced { reason: reason() },
        SandboxError::NetworkDenied {
            host: "h".into(),
            port: 1,
        },
        SandboxError::QuotaExceeded {
            used_bytes: 1,
            quota_bytes: 1,
        },
        SandboxError::PathOutsideWorkspace { path: reason() },
        SandboxError::FileTooLarge { size: 2, limit: 1 },
        SandboxError::NotARegularFile { path: reason() },
        SandboxError::OperationConflict(OperationId::new("o")),
        SandboxError::StaleGeneration {
            requested: Generation(1),
            current: Generation(2),
        },
        SandboxError::NotFound { what: reason() },
        SandboxError::Cancelled,
        SandboxError::Backend { reason: reason() },
        SandboxError::Repository(RepositoryError::WorkspaceNotFound(WorkspaceId::new("w"))),
    ]
}

#[test]
fn every_sandbox_code_has_a_failure_table_row() {
    let sink = [
        ExportSinkError::Refused { reason: "x".into() },
        ExportSinkError::Failed { reason: "x".into() },
    ];
    // Raised from a `match` or a tuple, out of the core scan's reach.
    let pack = [
        "plan_mode",
        "read_only",
        "write_failed",
        "tool_result_uncertain",
        "command_not_started",
    ];
    let service = service_errors();
    let codes = service
        .iter()
        .map(SandboxError::code)
        .chain(sink.iter().map(ExportSinkError::code))
        .chain(pack);
    for code in codes {
        assert!(
            agentkit_core::failure_catalog::spec(code).is_some(),
            "{code} has no row in core/src/failure_table*.rs"
        );
    }
}
