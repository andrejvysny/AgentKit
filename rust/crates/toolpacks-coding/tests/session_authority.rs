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
use agentkit_toolpacks_coding::types::{CodingMode, CodingPermissionAuthority};
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
    let requests = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(Scripted {
        requests: requests.clone(),
        count: AtomicUsize::new(0),
    });
    fixture_opts(Some("plan"), CodingMode::Plan, false, requests, provider).await
}

async fn fixture_opts(
    execution_mode: Option<&str>,
    guard_mode: CodingMode,
    pack_authorized: bool,
    requests: Arc<Mutex<Vec<agentkit_core::ChatRequest>>>,
    provider: Arc<dyn ChatProvider>,
) -> Fixture {
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
        permission: pack_authorized.then(|| {
            Arc::new(SessionExecutionAuthority::new(service.clone()))
                as Arc<dyn agentkit_toolpacks_coding::types::CodingPermissionAuthority>
        }),
    })
    .await
    .unwrap();
    let guard = CodingPermissionGuard::new(CodingPermissionGuardOptions {
        // No session state yet: the configured mode is the fallback. Plan is
        // the safest floor — the session must EARN write access.
        mode: guard_mode,
        role: None,
        authority: Some(Arc::new(SessionExecutionAuthority::new(service.clone()))),
        plan_tool_names: None,
    });
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(provider, Arc::new(ToolRegistry::new())).with_options(
            ChatExecutorOptions {
                tool_contributors: vec![pack],
                tool_guards: vec![Arc::new(guard)],
                ..Default::default()
            },
        ),
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
            execution_mode: execution_mode.map(str::to_owned),
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

/// Steps through fixed tool-call scripts, then answers `done`.
struct Steps {
    requests: Arc<Mutex<Vec<agentkit_core::ChatRequest>>>,
    steps: Vec<Vec<(&'static str, &'static str, serde_json::Value)>>,
    count: AtomicUsize,
}

#[async_trait]
impl ChatProvider for Steps {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(
        &self,
        input: agentkit_core::ChatRequest,
        _: CancellationToken,
    ) -> ProviderStream {
        self.requests.lock().unwrap().push(input);
        let n = self.count.fetch_add(1, Ordering::SeqCst);
        let calls: Vec<agentkit_core::ToolCall> = self
            .steps
            .get(n)
            .map(|step| {
                step.iter()
                    .map(|(id, name, args)| agentkit_core::ToolCall {
                        id: (*id).into(),
                        name: (*name).into(),
                        arguments_json: args.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if calls.is_empty() { "done" } else { "" }.into(),
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

async fn start_turn(f: &Fixture, text: &str) -> String {
    let sent = f
        .service
        .send(
            "s1",
            &agentkit_host::SendInput {
                content: json!(text),
                input_id: None,
                when_busy: WhenBusy::Reject,
                turn_id: None,
                on_settled: None,
            },
        )
        .await
        .unwrap();
    match sent {
        agentkit_host::AgentSendResult::Turn { turn_id, .. } => turn_id,
        agentkit_host::AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    }
}

#[tokio::test]
async fn a_plan_without_a_mode_omits_absent_optionals_and_plan_approved_judges_by_digest() {
    // A LEGAL submit_plan with no optional `mode`: the grant must persist
    // without the absent keys (never `"requestedMode": null`), the state
    // must stay readable, and `plan_approved` must judge THIS digest.
    let requests = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(Steps {
        requests: requests.clone(),
        steps: vec![
            vec![(
                "call-plan",
                "submit_plan",
                json!({"plan": "write a.txt with first"}),
            )],
            vec![],
        ],
        count: AtomicUsize::new(0),
    });
    let f = fixture_opts(Some("plan"), CodingMode::Plan, true, requests, provider).await;
    let turn = start_turn(&f, "plan it").await;
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn).await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    let request = pending_request(&f.store, &turn).await;
    assert_eq!(request.content["toolName"], "submit_plan");
    assert!(
        request.content.get("requestedMode").is_none(),
        "no mode was requested: no key, not a null"
    );

    f.service
        .approve(&request.input_id, "ans-plan", None, None)
        .await
        .unwrap();
    let state = f
        .service
        .get_execution("s1")
        .await
        .unwrap()
        .expect("state survives");
    assert_eq!(state["mode"], "ask");
    let grant = state["grantedPlan"].as_object().expect("a plan grant");
    assert!(grant.contains_key("planDigest"));
    assert!(
        !grant.contains_key("requestedMode"),
        "the absent optional was omitted, not nulled: {grant:?}"
    );

    // The resumed submit_plan executes under the pack's session authority
    // and reports the grant — `granted:true`, not the recorded-only default.
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let seen = f.requests.lock().unwrap().last().unwrap().clone();
    let tool = seen
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "tool")
        .expect("the executed plan answers the call");
    let envelope: serde_json::Value =
        serde_json::from_str(tool.content.as_str().expect("model envelope")).unwrap();
    assert_eq!(envelope["ok"], true, "{envelope}");
    assert_eq!(envelope["summary"], "plan approved", "{envelope}");
    assert!(
        envelope["data"]
            .as_str()
            .is_some_and(|text| text.contains("Execution mode is now ask")),
        "the model is told the granted mode: {envelope}"
    );
    // No synthetic "missing result" survives beside the real answer.
    assert!(
        !seen.messages.iter().any(|m| m.is_synthetic_missing()),
        "the answered call keeps only real evidence"
    );

    // Digest-specific judgement: the approved digest is Some(true), any
    // other digest is Some(false) (never the inherited `None`), and a chat
    // that is not a session is never judged.
    let authority = SessionExecutionAuthority::new(f.service.clone());
    let digest = grant["planDigest"].as_str().unwrap();
    assert_eq!(
        authority.plan_approved(Some("s1"), digest).await.unwrap(),
        Some(true)
    );
    assert_eq!(
        authority
            .plan_approved(Some("s1"), &"f".repeat(64))
            .await
            .unwrap(),
        Some(false),
        "a different plan is not this plan's grant"
    );
    assert_eq!(authority.plan_approved(None, digest).await.unwrap(), None);

    // A malformed grant is an error, never an absence the plan tool could
    // read as "no authority wired".
    let mut metadata = f.store.get_chat("s1").await.unwrap().unwrap().metadata;
    {
        let object = metadata.as_object_mut().unwrap();
        object.insert(
            "agentkit.session.execution".into(),
            json!({
                "version": 1,
                "mode": "ask",
                "updatedAt": "2026-01-01T00:00:00.000Z",
                "history": [],
                "grantedPlan": {"planDigest": null, "mode": "ask", "requestId": "r", "turnId": "t", "approvedAt": "a"},
            }),
        );
    }
    f.store
        .update_chat("s1", None, Some(metadata.clone()), None, 9_999_999_999_999)
        .await
        .unwrap();
    assert!(
        authority.plan_approved(Some("s1"), digest).await.is_err(),
        "a corrupt grant fails closed"
    );
    // ... and the legacy explicit-null optionals repair to absent.
    {
        let object = metadata.as_object_mut().unwrap();
        object.insert(
            "agentkit.session.execution".into(),
            json!({
                "version": 1,
                "mode": "ask",
                "updatedAt": "2026-01-01T00:00:00.000Z",
                "history": [],
                "grantedPlan": {"planDigest": digest, "mode": "ask", "requestId": "r", "turnId": "t", "approvedAt": "a", "requestedMode": null, "plan": null},
            }),
        );
    }
    f.store
        .update_chat("s1", None, Some(metadata), None, 9_999_999_999_999)
        .await
        .unwrap();
    let repaired = f
        .service
        .get_execution("s1")
        .await
        .unwrap()
        .expect("legacy nulls repair");
    assert_eq!(repaired["grantedPlan"]["planDigest"], digest);
    assert!(repaired["grantedPlan"].get("requestedMode").is_none());
    assert_eq!(
        authority.plan_approved(Some("s1"), digest).await.unwrap(),
        Some(true)
    );
    fs::remove_dir_all(&f.workspace).ok();
}

#[tokio::test]
async fn malformed_execution_state_fails_closed_under_a_permissive_fallback() {
    // The configured mode is AutoEdit — the DANGEROUS fallback. A corrupt
    // session state must error, never be read as "absent" and quietly run
    // the write under the configured permissive mode.
    let requests = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(Steps {
        requests: requests.clone(),
        steps: vec![
            vec![(
                "call-write",
                "write_file",
                json!({"path": "a.txt", "content": "sneaky"}),
            )],
            vec![],
        ],
        count: AtomicUsize::new(0),
    });
    let f = fixture_opts(None, CodingMode::AutoEdit, false, requests, provider).await;
    let mut metadata = f.store.get_chat("s1").await.unwrap().unwrap().metadata;
    let object = metadata.as_object_mut().unwrap();
    object.insert(
        "agentkit.session.execution".into(),
        json!({"version": 1, "mode": null, "updatedAt": "2026-01-01T00:00:00.000Z", "history": []}),
    );
    f.store
        .update_chat("s1", None, Some(metadata), None, 9_999_999_999_999)
        .await
        .unwrap();
    assert!(
        f.service.get_execution("s1").await.is_err(),
        "corrupt permission state is an error, not an absence"
    );

    let _turn = start_turn(&f, "write it").await;
    f.host.run_once().await.unwrap();
    assert!(
        !f.workspace.join("a.txt").exists(),
        "the AutoEdit fallback never ran the write under corrupt state"
    );
    let seen = f.requests.lock().unwrap();
    let tool_text: String = seen
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| m.role == "tool")
        .map(|m| m.content.to_string())
        .collect();
    assert!(
        tool_text.contains("tool_guard_refused"),
        "the call is refused by the guard chain, not run: {tool_text}"
    );
    drop(seen);
    fs::remove_dir_all(&f.workspace).ok();
}
