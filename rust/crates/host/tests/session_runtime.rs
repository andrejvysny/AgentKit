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
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = self.tool_first && self.requests.lock().unwrap().is_empty();
        self.requests.lock().unwrap().push(input);
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
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let calls = Arc::new(Mutex::new(vec![]));
    let requests = Arc::new(Mutex::new(vec![]));
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                tool_first,
                tool: "write_file",
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
