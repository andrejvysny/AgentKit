//! The coding pack governed by SESSION authority (M2-8): the session starts
//! in plan mode, the model is refused writes outright, escalates through an
//! approved plan (the durable grant flips the session mode), and only then a
//! per-call approval lets the real write tool touch the workspace — under a
//! durable receipt, with the human approving the exact bytes.

use agentkit_core::{ChatProvider, ChatRequest, ProviderEvent, ProviderStream, ToolRegistry};
use agentkit_host::{
    AgentHost, AgentSessionService, AgentSessionServiceDeps, CHAT_TURN_TASK_KIND, ChatExecutor,
    ChatExecutorOptions, CreateSessionInput, HostResult, PlanApprovalPolicy, SharedRuntime,
    SystemRuntime, TaskExecutor, WhenBusy,
};
use agentkit_store::{
    AgentInputKind, AgentInputQuery, AgentInputStatus, ExecutionReceiptStatus, SqliteStore,
    TaskStatus,
};
use agentkit_toolpacks_coding::guard::{CodingPermissionGuard, CodingPermissionGuardOptions};
use agentkit_toolpacks_coding::types::CodingMode;
use agentkit_toolpacks_coding::{
    CodingToolPackOptions, CodingWorkspace, SandboxMode, SessionExecutionAuthority,
    create_coding_tool_pack,
};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    count: AtomicUsize,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.requests.lock().unwrap().push(input);
        let n = self.count.fetch_add(1, Ordering::SeqCst);
        let (event, calls) = match n {
            0 => (
                "",
                vec![agentkit_core::ToolCall {
                    id: "call-refused".into(),
                    name: "write_file".into(),
                    arguments_json: json!({"path":"a.txt","content":"first"}).to_string(),
                }],
            ),
            1 => (
                "",
                vec![agentkit_core::ToolCall {
                    id: "call-plan".into(),
                    name: "submit_plan".into(),
                    arguments_json: json!({"plan":"write a.txt with first","mode":"ask"})
                        .to_string(),
                }],
            ),
            2 => (
                "",
                vec![agentkit_core::ToolCall {
                    id: "call-write".into(),
                    name: "write_file".into(),
                    arguments_json: json!({"path":"a.txt","content":"first"}).to_string(),
                }],
            ),
            _ => ("done", vec![]),
        };
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: event.into(),
            finish_reason: if calls.is_empty() {
                "stop"
            } else {
                "tool_calls"
            }
            .into(),
            tool_calls: calls,
        })]))
    }
}

struct ToAsk;

#[async_trait]
impl PlanApprovalPolicy for ToAsk {
    fn tool_names(&self) -> Vec<String> {
        vec!["submit_plan".into()]
    }
    async fn granted_mode(&self, _: &str) -> HostResult<String> {
        Ok("ask".into())
    }
}

struct Fixture {
    store: SqliteStore,
    host: AgentHost,
    service: AgentSessionService,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    workspace: std::path::PathBuf,
}

async fn fixture() -> Fixture {
    let workspace = std::env::temp_dir().join(format!(
        "agentkit-session-authority-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&workspace).unwrap();
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let requests = Arc::new(Mutex::new(vec![]));
    // The service exists BEFORE the host so the guard can read the session's
    // execution state; queue wake-up is driven by `run_once` in tests.
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: runtime.clone(),
        plan_approval: Some(Arc::new(ToAsk)),
        wake: None,
    });
    let pack = create_coding_tool_pack(CodingToolPackOptions {
        workspace: CodingWorkspace {
            root: workspace.display().to_string(),
        },
        limits: Default::default(),
        allow_sensitive: false,
        clock: None,
        sandbox: SandboxMode::Off,
        network: false,
        allow_git_writes: false,
        test_commands: vec![],
        platform: Some("linux".into()),
        sandbox_exec_path: None,
        coordinator: None,
        coordination_wait_ms: None,
        permission: None,
    })
    .await
    .unwrap();
    let guard = CodingPermissionGuard::new(CodingPermissionGuardOptions {
        // No session state yet: the configured mode is the fallback. Plan is
        // the safest floor — the session must EARN write access.
        mode: CodingMode::Plan,
        role: None,
        authority: Some(Arc::new(SessionExecutionAuthority::new(service.clone()))),
        plan_tool_names: None,
    });
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                count: AtomicUsize::new(0),
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![pack],
            tool_guards: vec![Arc::new(guard)],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime,
        agentkit_host::HostOptions::default(),
    )
    .unwrap();
    host.set_session_flush_planner(Arc::new(service.clone()))
        .await;
    service
        .create_session(&CreateSessionInput {
            session_id: Some("s1".into()),
            title: None,
            profile_id: "p1".into(),
            profile_digest: None,
            provider_id: None,
            model: Some("model".into()),
            workspace: None,
            context: None,
            skills: None,
            turn_kind: None,
            execution_mode: Some("plan".into()),
            metadata: None,
        })
        .await
        .unwrap();
    Fixture {
        store,
        host,
        service,
        requests,
        workspace,
    }
}

async fn pending_request(store: &SqliteStore, turn_id: &str) -> agentkit_store::AgentInputRecord {
    let requests = store
        .list_inputs(AgentInputQuery {
            session_id: "s1".into(),
            kinds: Some(vec![AgentInputKind::HumanRequest]),
            statuses: Some(vec![AgentInputStatus::Pending]),
            target_task_id: Some(turn_id.to_owned()),
        })
        .await
        .unwrap();
    assert_eq!(requests.len(), 1, "exactly one request awaits the answer");
    requests.into_iter().next().unwrap()
}

#[tokio::test]
async fn plan_mode_escalates_by_grant_and_the_approved_write_reaches_the_workspace() {
    let f = fixture().await;
    let sent = f
        .service
        .send(
            "s1",
            &agentkit_host::SendInput {
                content: json!("write a.txt with first"),
                input_id: None,
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    let turn_id = match sent {
        agentkit_host::AgentSendResult::Turn { turn_id, .. } => turn_id,
        agentkit_host::AgentSendResult::Queued { .. } => panic!(),
    };

    // Attempt 1: the plan-mode write is a FINAL refusal (no request was
    // owed), the model submits a plan, and the turn parks on its approval.
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    let plan_request = pending_request(&f.store, &turn_id).await;
    assert_eq!(plan_request.content["toolName"], "submit_plan");
    assert_eq!(plan_request.content["requestedMode"], "ask");

    // The plan approval is the ONLY escalation path: it flips the session
    // mode durably before the next model call.
    f.service
        .approve(&plan_request.input_id, "ans-plan", None, None)
        .await
        .unwrap();
    assert_eq!(
        f.service.get_execution("s1").await.unwrap().unwrap()["mode"],
        "ask"
    );
    f.host.run_once().await.unwrap();

    // Attempt 2: now in `ask` mode the write still needs a human — this is
    // a per-call approval, not the plan grant.
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    let write_request = pending_request(&f.store, &turn_id).await;
    assert_eq!(write_request.content["toolName"], "write_file");

    // Attempt 3: the exact approved call runs; the REAL pack tool touches
    // the workspace under its durable receipt.
    f.service
        .approve(&write_request.input_id, "ans-write", None, None)
        .await
        .unwrap();
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(
        fs::read_to_string(f.workspace.join("a.txt")).unwrap(),
        "first",
        "the approved bytes reached the workspace"
    );
    let receipts = f.store.list_executions(turn_id.clone()).await.unwrap();
    assert_eq!(receipts.len(), 1, "only the executed write is receipted");
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Completed);
    assert_eq!(
        receipts[0].approval_request_id.as_deref(),
        Some(write_request.input_id.as_str())
    );
    assert_eq!(receipts[0].tool_name, "write_file");

    // The first (plan-mode) refusal is visible to the model, and the final
    // answer followed the approved write.
    let requests = f.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let refusal = requests[1]
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.to_string())
        .collect::<String>();
    assert!(
        refusal.contains("propose the change with submit_plan"),
        "{refusal}"
    );
    fs::remove_dir_all(&f.workspace).ok();
}
