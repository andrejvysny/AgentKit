//! Approvals in a plain chat (no session descriptor), answered the way a host
//! answers them: `answer_human_request` as the owner. An approved call runs
//! once on resume, a denial reaches the model as a denial, a changed call is
//! refused on the record, and none of them reads as `tool_result_missing`.

use agentkit_chat::*;
use agentkit_core::{
    ChatMessage, ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};
use agentkit_governance::{
    ApprovalAsk, GovernanceResult, ToolGuard, ToolGuardContext, ToolGuardVerdict, args_sha256,
};
use agentkit_sessions::*;
use agentkit_store::{
    AgentInputKind, AgentInputQuery, AgentInputRecord, AgentInputStatus, CreateAgentInput,
    SqliteStore, TaskStatus,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

const CHAT: &str = "chat";
const TURN: &str = "t1";

/// Calls `forget` once, then answers with text.
struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(input);
            requests.len() == 1
        };
        let calls = if first {
            vec![agentkit_core::ToolCall {
                id: "call-1".into(),
                name: "forget".into(),
                arguments_json: "{\"id\":\"m1\"}".into(),
            }]
        } else {
            vec![]
        };
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if first { "" } else { "done" }.into(),
            finish_reason: if first { "tool_calls" } else { "stop" }.into(),
            tool_calls: calls,
        })]))
    }
}

struct Forget {
    definition: ToolDefinition,
    hits: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Forget {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"forgotten": "m1"})),
            sources: vec![],
            warnings: vec![],
            truncated: false,
            limits: context.limits,
            model_data: None,
            summary: None,
            status: None,
            model_content: None,
        })
    }
}

/// Every `forget` call needs the owner.
struct AskGuard;

#[async_trait]
impl ToolGuard for AskGuard {
    async fn can_execute(&self, context: &ToolGuardContext) -> GovernanceResult<ToolGuardVerdict> {
        if context.tool.name != "forget" {
            return Ok(ToolGuardVerdict::Allowed);
        }
        Ok(ToolGuardVerdict::AwaitingHuman(ApprovalAsk {
            reason: "approval_required: forget".into(),
            preview: None,
            requested_mode: None,
            uncertainty_generation: None,
        }))
    }
}

struct Fixture {
    store: SqliteStore,
    host: AgentHost,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

async fn parked() -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat(CHAT, None, json!({}), 0).await.unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": "forget", "version": "1", "effect": "write",
        "capability": "memory.write", "description": "forget a memory",
        "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}},
    }))
    .unwrap();
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Forget {
            definition,
            hits: hits.clone(),
        }))
        .unwrap();
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
            }),
            Arc::new(tools),
        )
        .with_options(ChatExecutorOptions {
            tool_guards: vec![Arc::new(AskGuard)],
            ..Default::default()
        }),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    host.submit_chat(SubmitChat {
        task_id: TURN.into(),
        chat_id: CHAT.into(),
        model: "model".into(),
        content: json!("forget m1"),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
    let fixture = Fixture {
        store,
        host,
        hits,
        requests,
    };
    assert_eq!(fixture.status().await, TaskStatus::WaitingApproval);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0, "parked, not run");
    fixture
}

impl Fixture {
    async fn status(&self) -> TaskStatus {
        self.store.get_task(TURN).await.unwrap().unwrap().status
    }

    async fn pending(&self) -> Vec<AgentInputRecord> {
        self.store
            .list_inputs(AgentInputQuery {
                session_id: CHAT.into(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(TURN.into()),
            })
            .await
            .unwrap()
    }

    /// The owner's answer, through the same store transaction a host uses.
    async fn answer(&self, request_id: &str, decision: &str) {
        let response = json!({ "decision": decision });
        let outcome = self
            .store
            .answer_human_request(AnswerHumanRequest {
                request_id: request_id.into(),
                answer_id: format!("ans:{request_id}"),
                response_hash: args_sha256(&response),
                response,
                now: now_ms(),
                message_id: format!("msg:{request_id}"),
                plan_tool_names: vec![],
                policy_denial: None,
                granted_mode: None,
                actor: AnswerActor::Owner,
            })
            .await
            .unwrap();
        assert!(outcome.enqueue.is_some(), "the answer requeues the turn");
    }

    async fn resume(&self) {
        self.host.run_once().await.unwrap();
        assert_eq!(self.status().await, TaskStatus::Completed);
    }

    /// The `call-1` result the model received on resume.
    fn result_seen_by_model(&self) -> Value {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "one request before the park, one after");
        let tools: Vec<&ChatMessage> = requests[1]
            .messages
            .iter()
            .filter(|message| {
                message.role == "tool" && message.tool_call_id.as_deref() == Some("call-1")
            })
            .collect();
        assert_eq!(tools.len(), 1, "exactly one result for the call: {tools:?}");
        assert!(!tools[0].is_synthetic_missing(), "{:?}", tools[0].content);
        match &tools[0].content {
            Value::String(text) => serde_json::from_str(text).unwrap(),
            value => value.clone(),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn error_code(result: &Value) -> Option<&str> {
    result["data"]["errorCode"].as_str()
}

#[tokio::test]
async fn an_approved_call_in_a_plain_chat_runs_once_on_resume() {
    let f = parked().await;
    let request = f.pending().await.into_iter().next().expect("one ask");
    assert_eq!(request.content["kind"], "approval");
    f.answer(&request.input_id, "approve").await;
    f.resume().await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 1, "the approved call ran");
    let result = f.result_seen_by_model();
    assert_eq!(error_code(&result), None, "{result}");
    assert!(result.to_string().contains("forgotten"), "{result}");
}

#[tokio::test]
async fn a_denied_call_in_a_plain_chat_reaches_the_model_as_a_denial() {
    let f = parked().await;
    let request = f.pending().await.into_iter().next().expect("one ask");
    f.answer(&request.input_id, "deny").await;
    f.resume().await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 0, "a denied call never runs");
    let result = f.result_seen_by_model();
    assert_eq!(error_code(&result), Some("approval_denied"), "{result}");
}

/// The call's terminal tool events in the run's durable log, as
/// `(kind, errorCode)`.
async fn tool_outcomes(f: &Fixture) -> Vec<(String, Option<String>)> {
    f.store
        .events(TURN, None, 10_000)
        .await
        .unwrap()
        .iter()
        .filter(|event| {
            let kind = event.event["type"].as_str().unwrap_or_default();
            (kind == "run.tool.succeeded" || kind == "run.tool.failed")
                && event.event["data"]["toolCallId"] == "call-1"
        })
        .map(|event| {
            (
                event.event["type"].as_str().unwrap_or_default().to_owned(),
                event.event["data"]["errorCode"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// H1.1c: a host renders tool results from the durable run events. A denial
/// is written as a tool record by the answer, so without an event the call
/// showed no result at all.
#[tokio::test]
async fn a_denied_call_has_one_failed_tool_event() {
    let f = parked().await;
    let request = f.pending().await.into_iter().next().expect("one ask");
    f.answer(&request.input_id, "deny").await;
    f.resume().await;
    assert_eq!(
        tool_outcomes(&f).await,
        vec![(
            "run.tool.failed".to_owned(),
            Some("approval_denied".to_owned())
        )]
    );
}

#[tokio::test]
async fn an_approved_call_has_one_succeeded_tool_event() {
    let f = parked().await;
    let request = f.pending().await.into_iter().next().expect("one ask");
    f.answer(&request.input_id, "approve").await;
    f.resume().await;
    assert_eq!(
        tool_outcomes(&f).await,
        vec![("run.tool.succeeded".to_owned(), None)]
    );
}

#[tokio::test]
async fn an_approval_for_other_arguments_never_runs_the_call() {
    let f = parked().await;
    // A second ask for the same call that approves DIFFERENT arguments.
    let changed = json!({"id": "m2"});
    f.store
        .create_input(
            CreateAgentInput {
                input_id: "hreq-mismatch".into(),
                session_id: CHAT.into(),
                kind: AgentInputKind::HumanRequest,
                content: json!({
                    "kind": "approval",
                    "toolName": "forget",
                    "argsSha256": args_sha256(&changed),
                    "prompt": "forget the other memory?",
                }),
                content_hash: args_sha256(&changed),
                target_task_id: Some(TURN.into()),
                on_settled: None,
                attempt_id: Some("attempt-1".into()),
                tool_call_id: Some("call-1".into()),
                expires_at: None,
            },
            now_ms(),
        )
        .await
        .unwrap();
    f.answer("hreq-mismatch", "approve").await;
    f.resume().await;
    assert_eq!(
        f.hits.load(Ordering::SeqCst),
        0,
        "a changed call never runs"
    );
    let result = f.result_seen_by_model();
    assert_eq!(error_code(&result), Some("approval_mismatch"), "{result}");
}
