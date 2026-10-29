//! End-to-end session runtime: a session turn that asks, parks, answers,
//! resumes the EXACT approved call, and hands the scope to the queued
//! follow-up — through the service, the executor, and the runner, with no
//! mocks below the store.

use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputStatus, AppendProjectedInputs,
    AppliedSteer, ClaimInput, MessageInput, RecoveryMode, SqliteStore, TaskStatus,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

/// `tool_first`: the first request declares one mutating call, every
/// request after answers. Without it every request just answers.
struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    tool_first: bool,
    tool: &'static str,
    fail: bool,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = self.tool_first && self.requests.lock().unwrap().is_empty();
        self.requests.lock().unwrap().push(input);
        if self.fail {
            return Box::pin(futures::stream::iter([Err(CoreError::new(
                "provider_error",
                "the provider said no",
            ))]));
        }
        let calls = if first {
            vec![agentkit_core::ToolCall {
                id: "call-1".into(),
                name: self.tool.into(),
                arguments_json: "{\"path\":\"a.txt\"}".into(),
            }]
        } else {
            vec![]
        };
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

struct WriteFile {
    definition: ToolDefinition,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for WriteFile {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.calls
            .lock()
            .unwrap()
            .push(input["path"].as_str().unwrap_or_default().to_owned());
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"written": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}

struct WriterContributor {
    tool: Arc<dyn Tool>,
}

#[async_trait]
impl ToolContributor for WriterContributor {
    fn namespace(&self) -> &str {
        "coding"
    }
    async fn contribute(&self, _: &ToolContributionContext) -> HostResult<Vec<ContributedTool>> {
        Ok(vec![ContributedTool::DirectWrite(self.tool.clone())])
    }
}

/// The coding-pack `ask` verdict in miniature: this call may run only after
/// a human said so.
struct AskGuard;

#[async_trait]
impl ToolGuard for AskGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> HostResult<ToolGuardVerdict> {
        if context.tool.name != "write_file" {
            return Ok(ToolGuardVerdict::Allowed);
        }
        Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
            reason: format!("approval_required: {}", context.tool.name),
            preview: None,
            requested_mode: None,
            uncertainty_generation: None,
        }))
    }
}

fn write_tool(calls: Arc<Mutex<Vec<String>>>) -> Arc<dyn Tool> {
    Arc::new(WriteFile {
        calls,
        definition: serde_json::from_value(json!({
            "name": "write_file", "version": "1", "effect": "write",
            "capability": "fs.write", "description": "write",
            "inputSchema": {"type": "object"},
        }))
        .unwrap(),
    })
}

struct Fixture {
    store: SqliteStore,
    host: AgentHost,
    service: AgentSessionService,
    calls: Arc<Mutex<Vec<String>>>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

async fn fixture() -> Fixture {
    fixture_with(true).await
}

/// `tool_first: false` gives a session that only ever answers: enough to
/// exercise interrupt/resume without the approval machinery in the way.
async fn fixture_with(tool_first: bool) -> Fixture {
    fixture_at(
        SqliteStore::memory(64).unwrap(),
        tool_first,
        false,
        HostOptions::default(),
    )
    .await
}

/// A session whose provider fails every request; the settling path must
/// then leave the queue exactly as it found it (ADR 0016 §4). The retry
/// backoff is collapsed so attempts exhaust inside the test's budget.
async fn fixture_failing() -> Fixture {
    fixture_at(
        SqliteStore::memory(64).unwrap(),
        false,
        true,
        HostOptions {
            retry_backoff: RetryBackoff {
                base_ms: 1,
                max_ms: 2,
                jitter_ratio: 0.0,
            },
            ..HostOptions::default()
        },
    )
    .await
}

async fn fixture_at(
    store: SqliteStore,
    tool_first: bool,
    fail: bool,
    options: HostOptions,
) -> Fixture {
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    fixture_with_runtime(store, runtime.clone(), runtime, tool_first, fail, options).await
}

/// The same fixture with each half's clock supplied separately. The session
/// service's clock is the seam a test parks an admission in: the admission's
/// last clock read is the one immediately before its store write, so holding
/// it there lets a settlement commit in between — the interleaving that used
/// to strand a follow-up.
async fn fixture_with_runtime(
    store: SqliteStore,
    host_runtime: SharedRuntime,
    service_runtime: SharedRuntime,
    tool_first: bool,
    fail: bool,
    options: HostOptions,
) -> Fixture {
    let calls = Arc::new(Mutex::new(vec![]));
    let requests = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                tool_first,
                tool: "write_file",
                fail,
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![Arc::new(WriterContributor {
                tool: write_tool(calls.clone()),
            })],
            tool_guards: vec![Arc::new(AskGuard)],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        host_runtime,
        options,
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime: service_runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
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
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    Fixture {
        store,
        host,
        service,
        calls,
        requests,
    }
}

fn send_input(content: Value, input_id: Option<&str>, when_busy: WhenBusy) -> SendInput {
    SendInput {
        content,
        input_id: input_id.map(str::to_owned),
        when_busy,
        turn_id: None,
        on_settled: None,
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
    assert_eq!(
        requests.len(),
        1,
        "exactly one request must await the answer"
    );
    requests.into_iter().next().unwrap()
}

#[tokio::test]
async fn an_asked_call_parks_answers_spends_once_and_the_queue_moves() {
    let f = fixture().await;
    let sent = f
        .service
        .send("s1", &send_input(json!("write it"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    };
    // A follow-up behind the live turn queues instead of racing it.
    let queued = f
        .service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("and summarize"),
                input_id: Some("in2".into()),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(queued, AgentSendResult::Queued { .. }),
        "a busy session stores the input instead"
    );

    f.host.run_once().await.unwrap(); // attempt 1 asks and parks
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::WaitingApproval
    );
    assert!(
        f.calls.lock().unwrap().is_empty(),
        "the body never ran unasked"
    );
    let request = pending_request(&f.store, &turn_id).await;
    assert_eq!(request.content["kind"], "approval");
    assert_eq!(request.content["toolName"], "write_file");
    assert_eq!(
        request.content["argsSha256"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        64
    );
    assert!(request.attempt_id.is_some());
    // The waiting event carries the approval fields (projection parity).
    let events = f.store.events(&turn_id, None, 1000).await.unwrap();
    let waiting = events
        .iter()
        .find(|event| event.event["type"] == "run.waiting")
        .expect("run.waiting is durable before the park");
    assert_eq!(waiting.event["data"]["kind"], "approval");
    assert_eq!(
        waiting.event["data"]["argsSha256"],
        request.content["argsSha256"]
    );

    f.service
        .approve(&request.input_id, "ans-1", None, None)
        .await
        .unwrap();
    f.host.run_once().await.unwrap(); // attempt 2 runs the exact call
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(&*f.calls.lock().unwrap(), &vec!["a.txt".to_owned()]);
    // The receipt spent the single-use approval.
    let receipts = f.store.list_executions_by_chat("s1".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].approval_request_id.as_deref(),
        Some(request.input_id.as_str())
    );
    assert_eq!(
        receipts[0].status,
        agentkit_store::ExecutionReceiptStatus::Completed
    );
    // The answer was consumed exactly once: a second answer id is a
    // conflict, and the call does not re-run.
    let conflict = f
        .service
        .answer(&AnswerInput {
            request_id: request.input_id.clone(),
            answer_id: "ans-2".into(),
            response: json!({"decision": "approve"}),
        })
        .await
        .unwrap_err();
    assert_eq!(conflict.code, "answer_conflict");
    assert_eq!(f.calls.lock().unwrap().len(), 1);

    // Settlement promoted the queued follow-up; running it completes the
    // queue without leftovers.
    assert!(f.host.run_once().await.unwrap(), "the promoted turn runs");
    let promoted = f.store.get_task("turn:in2").await.unwrap().unwrap();
    assert_eq!(promoted.status, TaskStatus::Completed);
    let state = f.service.get_state("s1").await.unwrap();
    assert!(state.pending.is_empty());
    assert!(state.active_turn.is_none());
}

#[tokio::test]
async fn a_denied_call_never_runs_and_the_turn_finishes_without_it() {
    let f = fixture().await;
    let sent = f
        .service
        .send("s1", &send_input(json!("write it"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!(),
    };
    f.host.run_once().await.unwrap();
    let request = pending_request(&f.store, &turn_id).await;
    f.service
        .deny(&request.input_id, "ans-1", Some("not today"))
        .await
        .unwrap();
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert!(f.calls.lock().unwrap().is_empty(), "a denial is final");
    assert!(
        f.store
            .list_executions_by_chat("s1".into())
            .await
            .unwrap()
            .is_empty(),
        "a denied call was never admitted"
    );
    // The model saw the denial as this call's answer: history stays balanced.
    let last = f.requests.lock().unwrap().pop().unwrap();
    let tool = last
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "tool")
        .expect("the denial answers the call");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    // The store writes the answer as an envelope STRING (TypeScript parity).
    let envelope: serde_json::Value =
        serde_json::from_str(tool.content.as_str().expect("envelope string")).unwrap();
    assert_eq!(envelope["data"]["errorCode"], "approval_denied");
}

#[tokio::test]
async fn an_interrupted_queued_turn_parks_and_resumes() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("question"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!(),
    };
    // Nothing is running yet: the request parks the queued turn directly.
    f.service.interrupt("s1", &turn_id).await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Interrupted
    );
    let state = f.service.get_state("s1").await.unwrap();
    assert_eq!(
        state.active_turn.as_ref().unwrap()["status"],
        serde_json::to_value(TaskStatus::Interrupted).unwrap()
    );
    f.service.resume("s1", &turn_id).await.unwrap();
    f.host.run_once().await.unwrap();
    let resumed = f.store.get_task(&turn_id).await.unwrap().unwrap();
    assert_eq!(resumed.status, TaskStatus::Completed);
}

// -- mid-run interruption -----------------------------------------------------

/// The mutating body blocks until the store records the interrupt request:
/// the human pressed stop while the effect was live.
struct SlowWrite {
    definition: ToolDefinition,
    store: SqliteStore,
    turn: Arc<Mutex<String>>,
    entered: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for SlowWrite {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let turn = self.turn.lock().unwrap().clone();
        loop {
            let parked = self
                .store
                .get_task(&turn)
                .await
                .map_err(|_| CoreError::new("store", "unreadable"))?
                .is_some_and(|task| task.interrupt_requested);
            if parked {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        // Give the runner's heartbeat time to turn the durable request into
        // the in-memory interruption token.
        tokio::time::sleep(std::time::Duration::from_millis(220)).await;
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"written": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}

async fn fixture_slow(
    entered: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
    turn: Arc<Mutex<String>>,
) -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let requests = Arc::new(Mutex::new(vec![]));
    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": "slow_write", "version": "1", "effect": "write",
        "capability": "fs.write", "description": "slow write",
        "inputSchema": {"type": "object"},
    }))
    .unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                tool_first: true,
                tool: "slow_write",
                fail: false,
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![Arc::new(WriterContributor {
                tool: Arc::new(SlowWrite {
                    definition,
                    store: store.clone(),
                    turn,
                    entered,
                    hits,
                }),
            })],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions {
            // Fast heartbeat so the durable interrupt reaches the
            // in-memory token while the body is still live.
            lease_ttl_ms: 150,
            ..Default::default()
        },
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
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
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    Fixture {
        store,
        host,
        service,
        calls: Arc::new(Mutex::new(vec![])),
        requests,
    }
}

#[tokio::test]
async fn a_mid_run_interrupt_parks_and_the_resumed_turn_finishes_without_replay() {
    let entered = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    let turn = Arc::new(Mutex::new(String::new()));
    let f = fixture_slow(entered.clone(), hits.clone(), turn.clone()).await;
    let sent = f
        .service
        .send(
            "s1",
            &send_input(json!("slowly please"), None, WhenBusy::Reject),
        )
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!(),
    };
    *turn.lock().unwrap() = turn_id.clone();

    let host = f.host.clone();
    let running = tokio::spawn(async move { host.run_once().await });
    for _ in 0..2500 {
        if entered.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(
        entered.load(std::sync::atomic::Ordering::SeqCst),
        "the body started"
    );
    f.service.interrupt("s1", &turn_id).await.unwrap();
    running.await.unwrap().unwrap();

    // The attempt ended on the interruption check and the runner parked the
    // task instead of settling it: events, records and the answered call all
    // survive.
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Interrupted
    );
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

    f.service.resume("s1", &turn_id).await.unwrap();
    f.host.run_once().await.unwrap();
    let resumed = f.store.get_task(&turn_id).await.unwrap().unwrap();
    assert_eq!(resumed.status, TaskStatus::Completed);
    // The interrupt boundary was a tool boundary: the answered call was NOT
    // re-executed, and the resumed attempt only asked the provider.
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(f.requests.lock().unwrap().len(), 2);
    let last = f.requests.lock().unwrap().last().unwrap().clone();
    let tool = last
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "tool" && message.tool_call_id.as_deref() == Some("call-1"))
        .expect("the completed call keeps its recorded result");
    // Tool records carry the model-facing envelope STRING (parity).
    let envelope: Value =
        serde_json::from_str(tool.content.as_str().expect("envelope string")).unwrap();
    assert_eq!(envelope["ok"], true);
    // The park kept the event log intact: attempt 1's tool records and the
    // resumed attempt's completion are all durable.
    let events = f.store.events(&turn_id, None, 1000).await.unwrap();
    let types: Vec<&str> = events
        .iter()
        .map(|event| event.event["type"].as_str().unwrap_or(""))
        .collect();
    assert!(types.contains(&"run.tool.running"), "{types:?}");
    assert!(types.contains(&"run.completed"), "{types:?}");
}

// -- live steering: exactly one durable placement per steer (C4) ----------

struct GatedWrite {
    definition: ToolDefinition,
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for GatedWrite {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !self.release.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        })
        .await
        .map_err(|_| CoreError::new("test_gate", "the gate was never released"))?;
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"written": true})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
        })
    }
}

/// A file-backed fixture whose single tool holds until the test releases
/// it: the steer is submitted and (if wanted) cancelled strictly between
/// the tool's entry and its boundary poll — no sleeps deciding outcomes.
async fn fixture_gated(
    path: &std::path::Path,
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
) -> Fixture {
    let store = SqliteStore::open(path, 64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let requests = Arc::new(Mutex::new(vec![]));
    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": "gated_write", "version": "1", "effect": "write",
        "capability": "fs.write", "description": "gated write",
        "inputSchema": {"type": "object"},
    }))
    .unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                tool_first: true,
                tool: "gated_write",
                fail: false,
            }),
            Arc::new(ToolRegistry::new()),
        )
        .with_options(ChatExecutorOptions {
            tool_contributors: vec![Arc::new(WriterContributor {
                tool: Arc::new(GatedWrite {
                    definition,
                    entered,
                    release,
                }),
            })],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        runtime.clone(),
        HostOptions::default(),
    )
    .unwrap();
    let service = AgentSessionService::new(AgentSessionServiceDeps {
        store: store.clone(),
        runtime,
        plan_approval: None,
        wake: Some(host.wake_handle()),
    });
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
            execution_mode: None,
            metadata: None,
        })
        .await
        .unwrap();
    Fixture {
        store,
        host,
        service,
        calls: Arc::new(Mutex::new(vec![])),
        requests,
    }
}

async fn wait_steer_state(
    store: &SqliteStore,
    turn: &str,
    want: AgentInputStatus,
) -> agentkit_store::AgentInputRecord {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let list = store
                .list_inputs(AgentInputQuery {
                    session_id: "s1".into(),
                    kinds: Some(vec![AgentInputKind::Steer]),
                    statuses: Some(vec![want]),
                    target_task_id: Some(turn.to_owned()),
                })
                .await
                .unwrap();
            if let Some(record) = list.into_iter().next() {
                return record;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the steer never reached {want:?}"))
}

async fn wait_settled(store: &SqliteStore, turn: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let task = store.get_task(turn).await.unwrap().unwrap();
            if task.status.is_terminal() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the turn never settled");
}

fn steer_sightings(requests: &[ChatRequest], text: &str) -> usize {
    requests
        .iter()
        .map(|request| {
            request
                .messages
                .iter()
                .filter(|message| message.role == "user" && message.content == json!(text))
                .count()
        })
        .sum()
}

#[tokio::test]
async fn a_live_steer_projects_exactly_once_now_and_after_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("steer.db");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let f = fixture_gated(&path, entered.clone(), release.clone()).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("expected an immediate turn"),
    };

    let host = f.host.clone();
    let running = tokio::spawn(async move { host.run_once().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !entered.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the tool body started");

    f.service
        .steer(
            "s1",
            &SteerInput {
                content: json!("left!"),
                input_id: Some("st-1".into()),
                turn_id: Some(turn_id.clone()),
                on_settled: None,
            },
        )
        .await
        .unwrap();
    // Durable and Pending against the live turn BEFORE any placement.
    wait_steer_state(&f.store, &turn_id, AgentInputStatus::Pending).await;
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    running.await.unwrap().unwrap();
    wait_settled(&f.store, &turn_id).await;

    // ONE durable user record for one steer — the event observer and the
    // placement owner can no longer each write their own.
    let messages = f.store.messages("s1", false).await.unwrap();
    let placed: Vec<_> = messages
        .iter()
        .filter(|message| message.metadata.get("steering").and_then(Value::as_bool) == Some(true))
        .collect();
    assert_eq!(placed.len(), 1, "exactly one steering record: {placed:?}");
    assert_eq!(placed[0].id, "steer:st-1", "deterministic placement id");
    let input = f.store.get_input("st-1".into()).await.unwrap().unwrap();
    assert_eq!(input.status, AgentInputStatus::Applied);
    assert_eq!(input.applied_message_id.as_deref(), Some("steer:st-1"));

    // The live turn saw it exactly once.
    assert_eq!(
        steer_sightings(&f.requests.lock().unwrap(), "left!"),
        1,
        "delivered to the running turn exactly once"
    );

    // And so does the NEXT turn — exactly once, not twice.
    f.service
        .send("s1", &send_input(json!("next"), None, WhenBusy::Reject))
        .await
        .unwrap();
    f.host.run_once().await.unwrap();
    // Scoped: a stashed ChatRequest keeps its budget Arc (and the durable
    // checkpoint's store handle) alive, so release it before the reopen.
    {
        let requests = f.requests.lock().unwrap().clone();
        let last = requests.last().expect("the next turn asked the provider");
        let in_last = last
            .messages
            .iter()
            .filter(|message| message.role == "user" && message.content == json!("left!"))
            .count();
        assert_eq!(in_last, 1, "the next turn sees the steer exactly once");
    }

    // Process restart changes nothing: the history still holds one record.
    // (The owner-file lock is released when the old actor drains; retry
    // until then — the analogue of the process fully exiting.)
    drop(f);
    let reopened = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match SqliteStore::open(&path, 64) {
                Ok(store) => return store,
                Err(agentkit_store::StoreError::AlreadyOwned) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => panic!("reopen failed: {error}"),
            }
        }
    })
    .await
    .expect("the old handle released ownership");
    let steering_records = reopened
        .messages("s1", false)
        .await
        .unwrap()
        .into_iter()
        .filter(|message| message.metadata.get("steering").and_then(Value::as_bool) == Some(true))
        .count();
    assert_eq!(
        steering_records, 1,
        "reopened history keeps exactly one placement"
    );
}

#[tokio::test]
async fn a_steer_cancelled_before_projection_leaves_no_applied_message() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("steer-cancel.db");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let f = fixture_gated(&path, entered.clone(), release.clone()).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("expected an immediate turn"),
    };

    let host = f.host.clone();
    let running = tokio::spawn(async move { host.run_once().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !entered.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the tool body started");

    f.service
        .steer(
            "s1",
            &SteerInput {
                content: json!("too late"),
                input_id: Some("st-2".into()),
                turn_id: Some(turn_id.clone()),
                on_settled: None,
            },
        )
        .await
        .unwrap();
    wait_steer_state(&f.store, &turn_id, AgentInputStatus::Pending).await;
    // The cancellation wins before the boundary poll: the projection's CAS
    // must lose and write NOTHING — no message, no applied receipt.
    f.store
        .transition_input(
            "st-2".into(),
            vec![AgentInputStatus::Pending],
            AgentInputStatus::Cancelled,
            agentkit_store::AgentInputPatch::default(),
            2_000_000_000_000,
        )
        .await
        .unwrap();
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    running.await.unwrap().unwrap();
    wait_settled(&f.store, &turn_id).await;

    assert_eq!(
        f.store
            .get_input("st-2".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Cancelled
    );
    assert!(
        f.store.get_message("steer:st-2").await.unwrap().is_none(),
        "the cancelled steer left no applied message behind"
    );
    assert_eq!(
        steer_sightings(&f.requests.lock().unwrap(), "too late"),
        0,
        "the model is never shown a steering that was cancelled away"
    );
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// The crash window between the placement transaction (Applied CAS + the
/// single projected record — the unit of record) and the event sink's
/// `run.input.applied` announcement must not strand the log behind the
/// history: the next attempt reconciles the announcement from the durable
/// pair. The residue below is exactly what a death between those two
/// store transactions leaves: an Applied input with its `steer:` record,
/// and no event.
#[tokio::test]
async fn a_crash_between_placement_and_announcement_backfills_the_event() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("steer-backfill.db");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let f = fixture_gated(&path, entered.clone(), release.clone()).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    };
    // Attempt 1 runs for real, then dies mid-tool-body: the boundary poll
    // that would place the steer never happens, and no announcement can
    // exist yet.
    let host = f.host.clone();
    let running = tokio::spawn(async move { host.run_once().await });
    let seen = entered.clone();
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        while !seen.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the tool body started");
    running.abort();
    f.store
        .recover(now_ms() + 601_000, 10, 0, RecoveryMode::Manual)
        .await
        .unwrap();
    f.service.resume("s1", &turn_id).await.unwrap();

    // The residue owner takes the claim and commits ONE placement exactly
    // as `SessionSteering::pending` does (Applied CAS + the single user
    // record in one fenced transaction) — then dies before the
    // announcement transaction ever ran.
    let claimed = f
        .store
        .claim(ClaimInput {
            owner_id: "crash".into(),
            now: now_ms(),
            ttl_ms: 600_000,
            kinds: vec![CHAT_TURN_TASK_KIND.into()],
            busy_scopes: vec![],
            max_attempts: 10,
        })
        .await
        .unwrap()
        .expect("the turn is queued");
    assert_eq!(claimed.task.task_id, turn_id);
    f.service
        .steer(
            "s1",
            &SteerInput {
                content: json!("left!"),
                input_id: Some("st-1".into()),
                turn_id: Some(turn_id.clone()),
                on_settled: None,
            },
        )
        .await
        .unwrap();
    f.store
        .append_projected_inputs(AppendProjectedInputs {
            fence: claimed.lease.fence(),
            events: vec![],
            messages: vec![],
            creates: vec![],
            applies: vec![AppliedSteer {
                input_id: "st-1".into(),
                message: MessageInput {
                    id: Some("steer:st-1".into()),
                    chat_id: "s1".into(),
                    run_id: Some(turn_id.clone()),
                    role: "user".into(),
                    content: json!("left!"),
                    metadata: json!({"steering": true, "inputId": "st-1"}),
                    ..MessageInput::default()
                },
            }],
            task_id: turn_id.clone(),
            now: now_ms(),
        })
        .await
        .unwrap();

    // The crash sweep reclaims the abandoned attempt; the resume replays
    // the turn — and reconciles the log on the way in.
    f.store
        .recover(now_ms() + 601_000, 10, 0, RecoveryMode::Manual)
        .await
        .unwrap();
    f.service.resume("s1", &turn_id).await.unwrap();
    f.host.run_once().await.unwrap();
    wait_settled(&f.store, &turn_id).await;
    let task = f.store.get_task(&turn_id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Completed, "{:?}", task.error);
    let input = f.store.get_input("st-1".into()).await.unwrap().unwrap();
    assert_eq!(input.status, AgentInputStatus::Applied);
    assert_eq!(input.applied_message_id.as_deref(), Some("steer:st-1"));
    let placed = f
        .store
        .messages("s1", false)
        .await
        .unwrap()
        .into_iter()
        .filter(|message| message.metadata.get("steering").and_then(Value::as_bool) == Some(true))
        .count();
    assert_eq!(
        placed, 1,
        "the crash leaves exactly one durable steer record"
    );

    let log = f
        .store
        .events_after_number(&turn_id, None, 10_000)
        .await
        .unwrap();
    let announced: Vec<_> = log
        .iter()
        .filter(|record| record.event["type"] == "run.input.applied")
        .map(|record| record.event.clone())
        .collect();
    assert_eq!(
        announced.len(),
        1,
        "reconciliation gives the applied steer exactly one announcement, log: {log:?}"
    );
    assert_eq!(announced[0]["data"]["inputId"], "st-1");
    assert_eq!(announced[0]["data"]["kind"], "steer");
    // The dead attempt's provider step is unrecoverable; the reconciled
    // announcement says so honestly (0 + marker), never a 1-based lie.
    assert_eq!(announced[0]["data"]["reconciled"], true);
    assert_eq!(announced[0]["data"]["step"], 0);
    // The backfill sits ahead of everything the resumed attempt emitted:
    // the run settling proves its own sequence was never rejected by the
    // store's strictly-increasing gate.
    let applied_seq = announced[0]["seq"].as_f64().unwrap();
    let last_seq = log.last().unwrap().event["seq"].as_f64().unwrap();
    assert!(
        applied_seq < last_seq,
        "the resumed attempt continues the log after the backfill; log: {:?}",
        log.iter()
            .map(|record| (record.seq, record.event["type"].clone()))
            .collect::<Vec<_>>()
    );
    // And the replayed attempt saw the steer exactly once.
    assert_eq!(
        steer_sightings(&f.requests.lock().unwrap(), "left!"),
        1,
        "the resumed turn sees the placed steer exactly once"
    );
}

/// Reconciliation is an idempotent projection pass, not a re-emitter: a
/// placement whose announcement already stands in the log gets no second
/// one on the next attempt (eventId/seq would double-announce the input
/// to every observer replaying the cursor).
#[tokio::test]
async fn reconciliation_never_announces_a_placed_steer_twice() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("steer-nodup.db");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let f = fixture_gated(&path, entered.clone(), release.clone()).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    };
    let host = f.host.clone();
    let running = tokio::spawn(async move { host.run_once().await });
    let seen = entered.clone();
    tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        while !seen.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the tool body started");
    running.abort();
    f.store
        .recover(now_ms() + 601_000, 10, 0, RecoveryMode::Manual)
        .await
        .unwrap();
    f.service.resume("s1", &turn_id).await.unwrap();
    let claimed = f
        .store
        .claim(ClaimInput {
            owner_id: "crash".into(),
            now: now_ms(),
            ttl_ms: 600_000,
            kinds: vec![CHAT_TURN_TASK_KIND.into()],
            busy_scopes: vec![],
            max_attempts: 10,
        })
        .await
        .unwrap()
        .expect("the turn is queued");
    assert_eq!(claimed.task.task_id, turn_id);
    f.service
        .steer(
            "s1",
            &SteerInput {
                content: json!("left!"),
                input_id: Some("st-1".into()),
                turn_id: Some(turn_id.clone()),
                on_settled: None,
            },
        )
        .await
        .unwrap();
    // Placement AND its announcement: the full pair, as the live path
    // commits it (placement transaction + the sink's event).
    let seq = f.store.next_sequence(&turn_id).await.unwrap();
    f.store
        .append_projected_inputs(AppendProjectedInputs {
            fence: claimed.lease.fence(),
            events: vec![json!({
                "type": "run.input.applied",
                "runId": turn_id,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "contractVersion": "1",
                "eventId": "evt-seeded",
                "seq": seq,
                "data": {"inputId": "st-1", "kind": "steer", "step": 1},
            })],
            messages: vec![],
            creates: vec![],
            applies: vec![AppliedSteer {
                input_id: "st-1".into(),
                message: MessageInput {
                    id: Some("steer:st-1".into()),
                    chat_id: "s1".into(),
                    run_id: Some(turn_id.clone()),
                    role: "user".into(),
                    content: json!("left!"),
                    metadata: json!({"steering": true, "inputId": "st-1"}),
                    ..MessageInput::default()
                },
            }],
            task_id: turn_id.clone(),
            now: now_ms(),
        })
        .await
        .unwrap();

    f.store
        .recover(now_ms() + 601_000, 10, 0, RecoveryMode::Manual)
        .await
        .unwrap();
    f.service.resume("s1", &turn_id).await.unwrap();
    f.host.run_once().await.unwrap();
    wait_settled(&f.store, &turn_id).await;

    let announced: Vec<_> = f
        .store
        .events_after_number(&turn_id, None, 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|record| record.event["type"] == "run.input.applied")
        .map(|record| record.event)
        .collect();
    assert_eq!(
        announced.len(),
        1,
        "the existing announcement stands; reconciliation adds nothing"
    );
    assert_eq!(announced[0]["eventId"], "evt-seeded");
    assert_eq!(announced[0]["data"]["inputId"], "st-1");
}

// -- C5: queue conversion and settlement are one transaction -----------------

async fn queued_turns(store: &SqliteStore, scope: &str) -> Vec<String> {
    store
        .list_tasks(Some(scope.into()))
        .await
        .unwrap()
        .into_iter()
        .filter(|task| task.status == TaskStatus::Queued)
        .map(|task| task.task_id)
        .collect()
}

/// The settling turn owns the whole promotion: terminal state, the next
/// task, its messages and the converted receipt commit together, and a
/// reopened store agrees with what the live one shows (ADR 0016 §5).
#[tokio::test]
async fn a_completed_turn_promotes_its_follow_up_in_one_commit_surviving_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("settle.db");
    let f = fixture_at(
        SqliteStore::open(&path, 64).unwrap(),
        false,
        false,
        HostOptions::default(),
    )
    .await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        _ => panic!("expected an immediate turn"),
    };
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("later"),
                input_id: Some("in2".into()),
            },
        )
        .await
        .unwrap();

    f.host.run_once().await.unwrap(); // executes AND promotes in one commit

    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let input = f.store.get_input("in2".into()).await.unwrap().unwrap();
    assert_eq!(input.status, AgentInputStatus::Converted);
    assert_eq!(input.applied_task_id.as_deref(), Some("turn:in2"));
    let next = f.store.get_task("turn:in2").await.unwrap().unwrap();
    assert_eq!(next.status, TaskStatus::Queued);
    let messages = f.store.messages("s1", false).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == "user" && message.content == json!("later"))
            .count(),
        1,
        "the converted turn's user message exists exactly once"
    );
    assert_eq!(
        queued_turns(&f.store, "s1").await,
        vec!["turn:in2".to_owned()],
        "exactly one next task"
    );

    // The live commit is the only commit: a crash (all handles gone, new
    // actor on the same file) sees the same complete promotion.
    // Scoped: a stashed ChatRequest keeps its budget Arc (and the durable
    // checkpoint's store handle) alive, so release it before the reopen.
    {
        let _seen = f.requests.lock().unwrap().len();
    }
    drop(f);
    let reopened = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match SqliteStore::open(&path, 64) {
                Ok(store) => return store,
                Err(agentkit_store::StoreError::AlreadyOwned) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => panic!("reopen failed: {error}"),
            }
        }
    })
    .await
    .expect("the old actor releases ownership");

    assert_eq!(
        reopened.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let input = reopened.get_input("in2".into()).await.unwrap().unwrap();
    assert_eq!(input.status, AgentInputStatus::Converted);
    assert_eq!(input.applied_task_id.as_deref(), Some("turn:in2"));
    assert_eq!(
        reopened.get_task("turn:in2").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    let messages = reopened.messages("s1", false).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == "user" && message.content == json!("later"))
            .count(),
        1,
        "reopened state shows no duplicated or half-written promotion"
    );
}

/// A FAILED turn promotes nothing (ADR 0016 §4): the follow-up stays
/// Pending and invisible to the queue until an explicit `flushQueue` —
/// which then converts it exactly once.
#[tokio::test]
async fn a_failed_turn_leaves_the_queue_pending_until_an_explicit_flush() {
    let f = fixture_failing().await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        _ => panic!("expected an immediate turn"),
    };
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("later"),
                input_id: Some("in2".into()),
            },
        )
        .await
        .unwrap();

    // Exhaust the transient retries; every settle must refuse to promote.
    let mut attempts = 0;
    while attempts < 20 {
        attempts += 1;
        f.host.run_once().await.ok();
        let task = f.store.get_task(&turn_id).await.unwrap().unwrap();
        if task.status.is_terminal() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let task = f.store.get_task(&turn_id).await.unwrap().unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Failed,
        "provider failure settles Failed"
    );
    let input = f.store.get_input("in2".into()).await.unwrap().unwrap();
    assert_eq!(
        input.status,
        AgentInputStatus::Pending,
        "a failed turn must not swallow the queue"
    );
    assert!(f.store.get_task("turn:in2").await.unwrap().is_none());
    assert!(queued_turns(&f.store, "s1").await.is_empty());

    // The explicit release converts it — exactly once.
    let flushed = f.service.flush_queue("s1").await.unwrap();
    match flushed {
        Some(AgentSendResult::Turn { turn_id, .. }) => assert_eq!(turn_id, "turn:in2"),
        other => panic!("flush must start the queued follow-up: {other:?}"),
    }
    assert_eq!(
        queued_turns(&f.store, "s1").await,
        vec!["turn:in2".to_owned()],
        "exactly one next task from the flush"
    );
    assert!(
        f.service.flush_queue("s1").await.unwrap().is_none(),
        "the converted input is never converted twice"
    );
    let input = f.store.get_input("in2".into()).await.unwrap().unwrap();
    assert_eq!(input.status, AgentInputStatus::Converted);
    let messages = f.store.messages("s1", false).await.unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == "user" && message.content == json!("later"))
            .count(),
        1
    );
}

/// A CANCELLED turn also promotes nothing — the user who cancelled did not
/// ask for the queued follow-up to start on its own.
#[tokio::test]
async fn a_cancelled_turn_leaves_the_queue_pending() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cancel.db");
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let f = fixture_gated(&path, entered.clone(), release.clone()).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("write it"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        _ => panic!("expected an immediate turn"),
    };
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("later"),
                input_id: Some("in2".into()),
            },
        )
        .await
        .unwrap();

    let host = f.host.clone();
    let running = tokio::spawn(async move { host.run_once().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !entered.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the tool body started");
    f.service.cancel("s1", &turn_id).await.unwrap();
    // The gate holds the tool body until the cancellation is durable, so
    // the settle is guaranteed to observe `cancel_requested` — no race
    // between cancel and the turn's completion.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let task = f.store.get_task(&turn_id).await.unwrap().unwrap();
            if task.cancel_requested {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the cancellation is recorded");
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    running.await.unwrap().unwrap();
    wait_settled(&f.store, &turn_id).await;

    let task = f.store.get_task(&turn_id).await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    let input = f.store.get_input("in2".into()).await.unwrap().unwrap();
    assert_eq!(
        input.status,
        AgentInputStatus::Pending,
        "cancelling a turn does not promote the queue"
    );
    assert!(f.store.get_task("turn:in2").await.unwrap().is_none());
}

/// An unusable queued record (empty content) is skipped at staging — it
/// stays Pending and visible, never aborts a healthy settle, and never
/// blocks the valid candidate behind it. A refused conversion writes
/// NOTHING (no task without messages, no receipt without a task).
#[tokio::test]
async fn a_poison_queued_input_never_wedges_the_settle_or_the_flush() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        _ => panic!("expected an immediate turn"),
    };
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!(""),
                input_id: Some("poison".into()),
            },
        )
        .await
        .unwrap();
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("real"),
                input_id: Some("in2".into()),
            },
        )
        .await
        .unwrap();

    // The poison is no longer picked as a candidate: the settle commits.
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed,
        "the settle survives an unusable queued record"
    );
    let poison = f.store.get_input("poison".into()).await.unwrap().unwrap();
    assert_eq!(
        poison.status,
        AgentInputStatus::Pending,
        "the unusable input stays Pending and visible"
    );
    assert_eq!(
        f.store
            .get_input("in2".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Converted,
        "the valid candidate behind it still promotes"
    );
    assert_eq!(
        queued_turns(&f.store, "s1").await,
        vec!["turn:in2".to_owned()]
    );

    // Running the promoted turn and then flushing must not stage the poison
    // as a task either — a refused flush writes nothing.
    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task("turn:in2").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let flushed = f.service.flush_queue("s1").await.unwrap();
    assert!(flushed.is_none(), "an unusable input cannot become a turn");
    assert_eq!(
        f.store
            .get_input("poison".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
    assert!(f.store.get_task("turn:poison").await.unwrap().is_none());
    assert!(queued_turns(&f.store, "s1").await.is_empty());
    let messages = f.store.messages("s1", false).await.unwrap();
    assert!(
        messages
            .iter()
            .all(|message| !(message.role == "user" && message.content == json!("")))
    );
}

/// Which settlement race to land: a follow-up that arrives after the
/// service snapshotted the queue, or the first of two supplied candidates
/// being cancelled after it.
enum Race {
    Arriving,
    CancelFirst,
    ModelAfterSnapshot,
    UnsupportedConfigAfterSnapshot,
    SupportedConfigAfterSnapshot,
}

/// Delegates to the real planner and then, in the window the reviewer
/// reproduced — after the snapshot is taken and before the settlement
/// transaction commits — lands the racing write. Nothing is mocked: the
/// same service, store and runner the field uses.
struct Racy {
    inner: AgentSessionService,
    store: SqliteStore,
    mode: Race,
    armed: AtomicBool,
}

#[async_trait]
impl SessionFlushPlanner for Racy {
    async fn settle_plan(
        &self,
        settling: &agentkit_store::TaskRecord,
        completed: bool,
    ) -> HostResult<agentkit_host::SettlePlan> {
        let plan = self.inner.settle_plan(settling, completed).await?;
        if completed && self.armed.swap(false, Ordering::SeqCst) {
            match self.mode {
                Race::Arriving => {
                    let sent = self
                        .inner
                        .follow_up(
                            "s1",
                            &FollowUpInput {
                                content: json!("and summarize"),
                                input_id: Some("late".into()),
                            },
                        )
                        .await
                        .expect("the follow-up queues behind the live turn");
                    assert!(
                        matches!(sent, AgentSendResult::Queued { .. }),
                        "the turn is still running when the input lands"
                    );
                }
                Race::CancelFirst => {
                    self.store
                        .transition_input(
                            "first".into(),
                            vec![AgentInputStatus::Pending],
                            AgentInputStatus::Cancelled,
                            AgentInputPatch::default(),
                            1_700_000_000_000,
                        )
                        .await
                        .expect("the queued candidate is cancelled");
                }
                Race::ModelAfterSnapshot => {
                    self.inner
                        .set_model("s1", None, "new-model", None)
                        .await
                        .unwrap();
                    let sent = self
                        .inner
                        .follow_up(
                            "s1",
                            &FollowUpInput {
                                content: json!("after the model change"),
                                input_id: Some("late-model".into()),
                            },
                        )
                        .await
                        .unwrap();
                    assert!(matches!(sent, AgentSendResult::Queued { .. }));
                }
                Race::UnsupportedConfigAfterSnapshot => {
                    self.inner
                        .configure("s1", None, Some(json!({"policy": "transcript"})), None)
                        .await
                        .unwrap();
                }
                Race::SupportedConfigAfterSnapshot => {
                    self.inner
                        .set_model("s1", None, "model", Some(None))
                        .await
                        .unwrap();
                    let sent = self
                        .inner
                        .follow_up(
                            "s1",
                            &FollowUpInput {
                                content: json!("after removing unsupported config"),
                                input_id: Some("late-supported".into()),
                            },
                        )
                        .await
                        .unwrap();
                    assert!(matches!(sent, AgentSendResult::Queued { .. }));
                }
            }
        }
        Ok(plan)
    }
}

/// Interleaving A: the follow-up arrives after the settlement snapshot but
/// before the running task settles. The settlement still owns the promotion
/// — the queue advances, it does not go quiet with a durable input no worker
/// will ever run.
#[tokio::test]
async fn a_follow_up_landing_after_the_settlement_snapshot_still_gets_its_turn() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("write it"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    };
    f.host
        .set_session_flush_planner(Arc::new(Racy {
            inner: f.service.clone(),
            store: f.store.clone(),
            mode: Race::Arriving,
            armed: AtomicBool::new(true),
        }))
        .await;

    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let late = f.store.get_input("late".into()).await.unwrap().unwrap();
    assert_eq!(
        late.status,
        AgentInputStatus::Converted,
        "an input that arrived mid-settlement is not left pending"
    );
    assert_eq!(
        queued_turns(&f.store, "s1").await,
        vec!["turn:late".to_owned()],
        "the settlement enqueues the next turn"
    );
    let promoted = f.store.get_task("turn:late").await.unwrap().unwrap();
    let user_message_id = promoted.payload["userMessageId"]
        .as_str()
        .unwrap()
        .to_owned();
    let assistant_message_id = promoted.payload["assistantMessageId"]
        .as_str()
        .unwrap()
        .to_owned();
    let messages = f.store.messages("s1", false).await.unwrap();
    assert!(
        messages.iter().any(|message| message.id == user_message_id
            && message.role == "user"
            && message.content == json!("and summarize")),
        "the promoted turn carries its user message"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.id == assistant_message_id
                && message.role == "assistant"
                && message.run_id.as_deref() == Some("turn:late")),
        "and its assistant placeholder"
    );
    assert_eq!(
        f.store
            .get_input("late".into())
            .await
            .unwrap()
            .unwrap()
            .applied_task_id
            .as_deref(),
        Some("turn:late"),
        "the receipt names the promoted turn"
    );

    // And it really runs: the queue drains to nothing.
    assert!(f.host.run_once().await.unwrap(), "the promoted turn runs");
    assert_eq!(
        f.store.get_task("turn:late").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let state = f.service.get_state("s1").await.unwrap();
    assert!(state.pending.is_empty());
    assert!(state.active_turn.is_none());
}

/// Interleaving B: the first of two supplied candidates is cancelled after
/// the snapshot. The settlement promotes the input behind it instead of
/// committing a terminal turn with a still-pending queue.
#[tokio::test]
async fn a_candidate_cancelled_after_the_snapshot_still_promotes_the_next() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("write it"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
    };
    for (id, text) in [("first", "and summarize"), ("second", "and count")] {
        assert!(
            matches!(
                f.service
                    .follow_up(
                        "s1",
                        &FollowUpInput {
                            content: json!(text),
                            input_id: Some(id.into()),
                        },
                    )
                    .await
                    .unwrap(),
                AgentSendResult::Queued { .. }
            ),
            "{id} queues behind the live turn"
        );
    }
    f.host
        .set_session_flush_planner(Arc::new(Racy {
            inner: f.service.clone(),
            store: f.store.clone(),
            mode: Race::CancelFirst,
            armed: AtomicBool::new(true),
        }))
        .await;

    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(
        f.store
            .get_input("first".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Cancelled
    );
    assert_eq!(
        f.store
            .get_input("second".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Converted,
        "the valid input behind a cancelled candidate still promotes"
    );
    assert!(
        f.store.get_task("turn:first").await.unwrap().is_none(),
        "a cancelled input never becomes a turn"
    );
    assert_eq!(
        queued_turns(&f.store, "s1").await,
        vec!["turn:second".to_owned()]
    );

    assert!(f.host.run_once().await.unwrap(), "the promoted turn runs");
    assert_eq!(
        f.store
            .get_task("turn:second")
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Completed
    );
    let state = f.service.get_state("s1").await.unwrap();
    assert!(state.pending.is_empty());
    assert!(state.active_turn.is_none());
}

/// The service's clock as a one-shot park. `arm(n)` blocks the n-th clock read
/// of the next operation until `release()`. The clock a queued admission reads
/// last is the one immediately before its store write, so parking there holds
/// the whole of the old check-then-write gap — after the store refused a
/// second turn, before the input is durable. The count is calibrated by
/// running one identical admission first, which keeps the park on the
/// operation's LAST read instead of an incidental one.
struct Barriered {
    fired: AtomicUsize,
    limit: AtomicUsize,
    armed: AtomicBool,
    entered: AtomicBool,
    release: AtomicBool,
}

impl Barriered {
    fn new() -> Self {
        Self {
            fired: AtomicUsize::new(0),
            limit: AtomicUsize::new(usize::MAX),
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            release: AtomicBool::new(false),
        }
    }

    fn arm(&self, limit: usize) {
        self.fired.store(0, Ordering::SeqCst);
        self.limit.store(limit, Ordering::SeqCst);
        self.entered.store(false, Ordering::SeqCst);
        self.release.store(false, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.release.store(true, Ordering::SeqCst);
    }

    /// Stop counting and report how many reads the operation made.
    fn count(&self) -> usize {
        self.armed.store(false, Ordering::SeqCst);
        self.fired.swap(0, Ordering::SeqCst)
    }
}

impl Runtime for Barriered {
    fn now_ms(&self) -> i64 {
        if self.armed.load(Ordering::SeqCst) {
            let seen = self.fired.fetch_add(1, Ordering::SeqCst) + 1;
            if seen >= self.limit.load(Ordering::SeqCst) {
                self.armed.store(false, Ordering::SeqCst);
                self.entered.store(true, Ordering::SeqCst);
                while !self.release.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        SystemRuntime.now_ms()
    }

    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn random(&self) -> f64 {
        0.5
    }
}

async fn wait_flag(flag: &AtomicBool, what: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !flag.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// The assessor's stranded follow-up, with no process tricks: the admission
/// parks on the clock read immediately before its store write, the settlement
/// commits across that park (re-reading a queue this input is not in yet), and
/// the admission then lands. It must NOT end as `first=Completed`,
/// `late=Pending`, no next task, idle worker: the write that inserts the input
/// is the write that decides whether it also becomes the next turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follow_up_whose_admission_straddles_the_settlement_is_never_stranded() {
    let barrier = Arc::new(Barriered::new());
    let f = fixture_with_runtime(
        SqliteStore::memory(64).unwrap(),
        Arc::new(SystemRuntime),
        barrier.clone(),
        false,
        false,
        HostOptions::default(),
    )
    .await;
    let sent = f
        .service
        .send("s1", &send_input(json!("work"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        _ => panic!("an idle session starts the turn"),
    };

    // Calibrate on an identical, unblocked admission.
    barrier.arm(usize::MAX);
    let probe = f
        .service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("probe"),
                input_id: Some("probe".into()),
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(probe, AgentSendResult::Queued { .. }),
        "a busy session queues"
    );
    let reads = barrier.count();
    assert!(reads > 0, "an admission reads the clock");
    assert!(f.service.cancel_queued_input("s1", "probe").await.unwrap());
    assert!(f.service.get_state("s1").await.unwrap().pending.is_empty());

    // Park the real admission, then settle across the park.
    barrier.arm(reads);
    let service = f.service.clone();
    let late = tokio::spawn(async move {
        service
            .follow_up(
                "s1",
                &FollowUpInput {
                    content: json!("and summarize"),
                    input_id: Some("late".into()),
                },
            )
            .await
    });
    wait_flag(
        &barrier.entered,
        "the admission reached its parked clock read",
    )
    .await;

    assert!(
        f.host.run_once().await.unwrap(),
        "turn 1 executes while the admission is parked"
    );
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert!(
        f.store.get_input("late".into()).await.unwrap().is_none(),
        "the settlement re-read a queue that did not yet contain this input"
    );

    barrier.release();
    let result = late.await.unwrap().unwrap();
    assert!(
        matches!(result, AgentSendResult::Turn { .. }),
        "an input that lands behind a terminal turn stages itself: {result:?}"
    );
    let input = f
        .store
        .get_input("late".into())
        .await
        .unwrap()
        .expect("the input is durable");
    assert_eq!(
        input.status,
        AgentInputStatus::Converted,
        "never Pending behind an already-terminal turn"
    );
    assert_eq!(input.applied_task_id.as_deref(), Some("turn:late"));
    assert_eq!(
        queued_turns(&f.store, "s1").await,
        vec!["turn:late".to_owned()],
        "a next task exists the moment the admission commits"
    );

    assert!(
        f.host.run_once().await.unwrap(),
        "the worker is not idle: it claims what the admission staged"
    );
    assert_eq!(
        f.store.get_task("turn:late").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let state = f.service.get_state("s1").await.unwrap();
    assert!(state.pending.is_empty());
    assert!(state.active_turn.is_none());
}

/// The staging contract in the field. A model change that lands while a turn
/// runs must reach the turn ITS settlement promotes — including the input the
/// host's snapshot could not name, which the store stages. Templating that
/// turn off the settling task's payload made the next provider request ask for
/// the model the session had already replaced (`[old, old]`).
#[tokio::test]
async fn a_promotion_the_snapshot_missed_asks_the_provider_with_the_current_model() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("write it"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let turn_id = match sent {
        AgentSendResult::Turn { turn_id, .. } => turn_id,
        _ => panic!("an idle session starts the turn"),
    };
    // `Arriving` lands the follow-up AFTER the settlement snapshot, so the
    // store — not the host — stages this promotion.
    f.host
        .set_session_flush_planner(Arc::new(Racy {
            inner: f.service.clone(),
            store: f.store.clone(),
            mode: Race::Arriving,
            armed: AtomicBool::new(true),
        }))
        .await;
    f.service
        .set_model("s1", None, "new-model", None)
        .await
        .unwrap();

    f.host.run_once().await.unwrap();
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let promoted = f
        .store
        .get_task("turn:late")
        .await
        .unwrap()
        .expect("the missed input was promoted");
    assert_eq!(
        promoted.payload["model"],
        json!("new-model"),
        "the promoted turn is stamped with the CURRENT descriptor"
    );
    assert_eq!(promoted.payload["chatId"], json!("s1"));

    assert!(f.host.run_once().await.unwrap(), "the promoted turn runs");
    let models: Vec<String> = f
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.model.clone())
        .collect();
    assert_eq!(
        models,
        vec!["model".to_owned(), "new-model".to_owned()],
        "the settling turn keeps the model it was submitted with and the \
         promoted turn asks the model the session uses now"
    );
    assert_eq!(
        f.store.get_task("turn:late").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}

/// A descriptor snapshot is not a durable task. A model change can finish
/// AFTER planning and before a newly accepted follow-up is staged, so the
/// settlement must validate its descriptor in the same transaction as the
/// promotion and re-plan without replaying the completed executor.
#[tokio::test]
async fn a_model_change_after_planning_reaches_the_later_follow_up() {
    let f = fixture_with(false).await;
    f.service
        .send("s1", &send_input(json!("first"), None, WhenBusy::Reject))
        .await
        .unwrap();
    f.host
        .set_session_flush_planner(Arc::new(Racy {
            inner: f.service.clone(),
            store: f.store.clone(),
            mode: Race::ModelAfterSnapshot,
            armed: AtomicBool::new(true),
        }))
        .await;
    assert!(f.host.run_once().await.unwrap());
    let promoted = f.store.get_task("turn:late-model").await.unwrap().unwrap();
    assert_eq!(promoted.payload["model"], json!("new-model"));
    assert_eq!(
        f.requests.lock().unwrap().len(),
        1,
        "re-planning never reruns the executor"
    );
    assert!(f.host.run_once().await.unwrap());
    let models: Vec<String> = f
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request.model.clone())
        .collect();
    assert_eq!(models, vec!["model".to_owned(), "new-model".to_owned()]);
}

/// Checking only model/provider keys would still accept an obsolete plan
/// after the session acquired unsupported context options. A refreshed plan
/// must use the same host validation as submission and leave the input
/// visible instead of dispatching a turn that silently ignores those options.
#[tokio::test]
async fn a_config_change_after_planning_revalidates_queued_turns() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("first"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("turn")
    };
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("next"),
                input_id: Some("next".into()),
            },
        )
        .await
        .unwrap();
    f.host
        .set_session_flush_planner(Arc::new(Racy {
            inner: f.service.clone(),
            store: f.store.clone(),
            mode: Race::UnsupportedConfigAfterSnapshot,
            armed: AtomicBool::new(true),
        }))
        .await;
    assert!(f.host.run_once().await.unwrap());
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert!(f.store.get_task("turn:next").await.unwrap().is_none());
    assert_eq!(
        f.store
            .get_input("next".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
    assert_eq!(
        f.service.flush_queue("s1").await.unwrap_err().code,
        "not_implemented"
    );
    assert_eq!(f.requests.lock().unwrap().len(), 1);
}

/// Taking (or rejecting) a settlement plan must not cancel an input before
/// its target becomes terminal. Cancellation belongs to the settle commit.
#[tokio::test]
async fn planning_a_settlement_does_not_cancel_its_steers() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("first"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("turn")
    };
    f.service
        .steer(
            "s1",
            &SteerInput {
                content: json!("only this turn"),
                input_id: Some("steer-cancel".into()),
                turn_id: Some(turn_id.clone()),
                on_settled: Some(OnSettled::Cancel),
            },
        )
        .await
        .unwrap();
    let task = f.store.get_task(&turn_id).await.unwrap().unwrap();
    let _ = f.service.settle_plan(&task, true).await.unwrap();
    assert_eq!(
        f.store
            .get_input("steer-cancel".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
}

/// A refusal snapshot also needs a descriptor fence. Otherwise a config
/// repaired before a valid follow-up is accepted leaves that input behind
/// the terminal turn with no promotion plan and no future settlement.
#[tokio::test]
async fn a_repaired_config_after_planning_does_not_strand_a_later_input() {
    let f = fixture_with(false).await;
    f.service
        .send("s1", &send_input(json!("first"), None, WhenBusy::Reject))
        .await
        .unwrap();
    f.service
        .set_model("s1", None, "model", Some(Some("high")))
        .await
        .unwrap();
    f.host
        .set_session_flush_planner(Arc::new(Racy {
            inner: f.service.clone(),
            store: f.store.clone(),
            mode: Race::SupportedConfigAfterSnapshot,
            armed: AtomicBool::new(true),
        }))
        .await;
    assert!(f.host.run_once().await.unwrap());
    let task = f
        .store
        .get_task("turn:late-supported")
        .await
        .unwrap()
        .expect("the accepted input is staged");
    assert!(task.payload.get("reasoningEffort").is_none());
    assert_eq!(
        f.store
            .get_input("late-supported".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Converted
    );
    assert!(f.host.run_once().await.unwrap());
    assert_eq!(f.requests.lock().unwrap().len(), 2);
    assert!(f.service.get_state("s1").await.unwrap().pending.is_empty());
}

struct ChangingEveryPlan {
    service: AgentSessionService,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl SessionFlushPlanner for ChangingEveryPlan {
    async fn settle_plan(
        &self,
        settling: &agentkit_store::TaskRecord,
        completed: bool,
    ) -> HostResult<SettlePlan> {
        let plan = self.service.settle_plan(settling, completed).await?;
        let calls = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.service
            .set_model("s1", None, &format!("changing-{calls}"), None)
            .await
            .unwrap();
        Ok(plan)
    }
}

/// Continuous edits produce an explicit bounded conflict. Neither the
/// completed executor nor any queue transition is replayed during re-planning;
/// the caller receives the error and can retry settlement/recover the durable
/// task instead of observing false completion or an indefinitely busy loop.
#[tokio::test]
async fn continuous_config_changes_return_a_bounded_conflict_without_partial_settlement() {
    let f = fixture_with(false).await;
    let sent = f
        .service
        .send("s1", &send_input(json!("first"), None, WhenBusy::Reject))
        .await
        .unwrap();
    let AgentSendResult::Turn { turn_id, .. } = sent else {
        panic!("turn")
    };
    f.service
        .follow_up(
            "s1",
            &FollowUpInput {
                content: json!("next"),
                input_id: Some("next".into()),
            },
        )
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    f.host
        .set_session_flush_planner(Arc::new(ChangingEveryPlan {
            service: f.service.clone(),
            calls: calls.clone(),
        }))
        .await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), f.host.run_once())
        .await
        .expect("configuration churn has a bounded outcome");
    assert!(matches!(
        result,
        Err(HostError::Store(
            agentkit_store::StoreError::SessionDescriptorChanged(_)
        ))
    ));
    assert!(
        calls.load(Ordering::SeqCst) > 1,
        "the planner retries a stale snapshot"
    );
    assert_eq!(
        f.requests.lock().unwrap().len(),
        1,
        "settlement retries never rerun the executor"
    );
    assert_eq!(
        f.store.get_task(&turn_id).await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(
        f.store
            .get_input("next".into())
            .await
            .unwrap()
            .unwrap()
            .status,
        AgentInputStatus::Pending
    );
    assert!(f.store.get_task("turn:next").await.unwrap().is_none());
}

struct Maintenance;
#[async_trait]
impl TaskExecutor for Maintenance {
    async fn execute(&self, _: ExecutionContext) -> HostResult<()> {
        Ok(())
    }
}

/// An unrelated task can hold the same scope, but its settlement does not
/// drive the session queue. Admission must not promise a follow-up that will
/// have no future session settlement to promote it.
#[tokio::test]
async fn a_different_task_kind_cannot_accept_a_session_follow_up() {
    let f = fixture_with(false).await;
    let maintenance = AgentHost::new(
        f.store.clone(),
        HashMap::from([(
            "project.maintenance".to_owned(),
            Arc::new(Maintenance) as Arc<dyn TaskExecutor>,
        )]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    maintenance
        .set_session_flush_planner(Arc::new(f.service.clone()))
        .await;
    f.store
        .submit(agentkit_store::SubmitTask {
            task_id: "maintenance".into(),
            kind: "project.maintenance".into(),
            scope_id: "s1".into(),
            payload: json!({}),
            priority: 0,
            now: SystemRuntime.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        })
        .await
        .unwrap();
    let input = FollowUpInput {
        content: json!("next"),
        input_id: Some("after-maintenance".into()),
    };
    let error = f.service.follow_up("s1", &input).await.unwrap_err();
    assert_eq!(error.code, "chat_busy");
    assert!(
        f.store
            .get_input("after-maintenance".into())
            .await
            .unwrap()
            .is_none(),
        "refusal writes no stranded input"
    );
    assert!(maintenance.run_once().await.unwrap());
    assert_eq!(
        f.store
            .get_task("maintenance")
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Completed
    );
    assert!(matches!(
        f.service.follow_up("s1", &input).await.unwrap(),
        AgentSendResult::Turn { .. }
    ));
    assert!(f.host.run_once().await.unwrap());
    assert_eq!(
        f.store
            .get_task("turn:after-maintenance")
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Completed
    );
}
