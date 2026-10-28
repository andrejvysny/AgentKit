//! End-to-end session runtime: a session turn that asks, parks, answers,
//! resumes the EXACT approved call, and hands the scope to the queued
//! follow-up — through the service, the executor, and the runner, with no
//! mocks below the store.

use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{AgentInputKind, AgentInputQuery, AgentInputStatus, SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize},
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
        runtime.clone(),
        options,
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
