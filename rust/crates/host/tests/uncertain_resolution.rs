//! Operator resolution of uncertain execution receipts (ADR 0020): a static
//! write tool whose body errors leaves an admitted receipt that blocks the
//! identical call in later runs until an operator explicitly resolves it.

use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{
    ClaimInput, ExecutionReceiptAdmission, ExecutionReceiptStatus, MessageInput, ResolutionOutcome,
    SettleTask, SqliteStore, TaskStatus,
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

const ARGS: &str = "{\"path\":\"a.txt\"}";

/// Declares one `mutate` call per turn (a fresh id each turn) while armed.
struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    call_seq: Arc<AtomicUsize>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let answered = input.messages.last().is_some_and(|m| m.role == "tool");
        self.requests.lock().unwrap().push(input);
        let calls = if answered {
            vec![]
        } else {
            let n = self.call_seq.fetch_add(1, Ordering::SeqCst) + 1;
            vec![agentkit_core::ToolCall {
                id: format!("call-{n}"),
                name: "mutate".into(),
                arguments_json: ARGS.into(),
            }]
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

struct Mutate {
    definition: ToolDefinition,
    hits: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for Mutate {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        // An error is not proof of no effect: the receipt must stay admitted.
        if self.fail.load(Ordering::SeqCst) {
            return Err(CoreError::new("boom", "the body failed midway"));
        }
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"done": true})),
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

struct Fixture {
    store: SqliteStore,
    host: AgentHost,
    service: AgentSessionService,
    hits: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

async fn fixture() -> Fixture {
    let store = SqliteStore::memory(64).unwrap();
    let runtime: SharedRuntime = Arc::new(SystemRuntime);
    let hits = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicBool::new(true));
    let requests = Arc::new(Mutex::new(vec![]));
    let definition: ToolDefinition = serde_json::from_value(json!({
        "name": "mutate", "version": "1", "effect": "write",
        "capability": "fs.write", "description": "mutate",
        "inputSchema": {"type": "object"},
    }))
    .unwrap();
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Mutate {
            definition,
            hits: hits.clone(),
            fail: fail.clone(),
        }))
        .unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(
            Arc::new(Scripted {
                requests: requests.clone(),
                call_seq: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(tools),
        )
        .with_options(ChatExecutorOptions::default()),
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
    for id in ["s1", "s2"] {
        service
            .create_session(&CreateSessionInput {
                session_id: Some(id.into()),
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
    }
    Fixture {
        store,
        host,
        service,
        hits,
        fail,
        requests,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

impl Fixture {
    /// A claimed run of `s1` with an admitted `mutate` receipt for `call`.
    /// `settle` ends the run as a crash would, declaring the call in history.
    async fn seed_admitted(&self, run_id: &str, call: &str, settle: bool) -> String {
        self.host
            .submit_chat(SubmitChat {
                task_id: run_id.into(),
                chat_id: "s1".into(),
                model: "model".into(),
                content: json!("work"),
                parent_message_id: None,
            })
            .await
            .unwrap();
        let claimed = self
            .store
            .claim(ClaimInput {
                owner_id: "crash".into(),
                now: now_ms(),
                ttl_ms: 600_000,
                kinds: vec![CHAT_TURN_TASK_KIND.into()],
                busy_scopes: vec![],
                max_attempts: 5,
            })
            .await
            .unwrap()
            .expect("queued run");
        let fence = claimed.lease.fence();
        let execution = agentkit_host::execution_id(run_id, call, None);
        let args = serde_json::from_str::<Value>(ARGS).unwrap();
        self.store
            .admit_execution(
                fence.clone(),
                ExecutionReceiptAdmission {
                    execution_id: execution.clone(),
                    run_id: run_id.into(),
                    chat_id: "s1".into(),
                    tool_call_id: call.into(),
                    tool_name: "mutate".into(),
                    args_sha256: args_sha256(&args),
                    ts_args_sha256: Some(ts_args_sha256(&args)),
                    args_hash_provenance: agentkit_store::ArgsHashProvenance::Canonical,
                    context_sha256: "0".repeat(64),
                    attempt_id: "1".into(),
                    approval_request_id: None,
                    approval_generation: None,
                    now: now_ms(),
                },
            )
            .await
            .unwrap();
        if !settle {
            return execution;
        }
        self.store
            .append_message(
                MessageInput {
                    id: Some(format!("{run_id}-assistant")),
                    chat_id: "s1".into(),
                    run_id: Some(run_id.into()),
                    role: "assistant".into(),
                    content: json!("working"),
                    parent_message_id: None,
                    tool_call_id: None,
                    tool_calls: Some(json!([
                        {"id": call, "name": "mutate", "argumentsJson": ARGS}
                    ])),
                    model_result_json: None,
                    metadata: json!({}),
                    activate: true,
                    explicit_root: false,
                },
                None,
                now_ms(),
            )
            .await
            .unwrap();
        self.store
            .bind_run(
                run_id,
                "s1",
                json!({
                    "providerId": "test", "protocol": "responses",
                    "connectionId": "c1", "generation": 1,
                    "chatId": "s1", "branchId": "b1", "model": "model",
                }),
                fence.clone(),
                now_ms(),
            )
            .await
            .unwrap();
        self.store
            .settle(SettleTask {
                fence,
                now: now_ms(),
                status: TaskStatus::Failed,
                error: Some("simulated process death".into()),
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
        execution
    }

    async fn turn(&self, id: &str) -> String {
        let result = self
            .service
            .send(
                "s1",
                &SendInput {
                    content: json!("mutate it"),
                    input_id: Some(id.into()),
                    when_busy: WhenBusy::Reject,
                    turn_id: None,
                    on_settled: None,
                },
            )
            .await
            .unwrap();
        match result {
            AgentSendResult::Turn { turn_id, .. } => turn_id,
            AgentSendResult::Queued { .. } => panic!("an idle session starts the turn"),
        }
    }

    /// Run one turn to its end and return the last `mutate` tool message of
    /// the final model request.
    async fn run_turn(&self, id: &str) -> (String, Option<Value>) {
        let turn = self.turn(id).await;
        self.host.run_once().await.unwrap();
        let last = self.requests.lock().unwrap().last().cloned();
        let tool = last.and_then(|request| {
            request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "tool")
                .map(|m| {
                    serde_json::from_str(m.content.as_str().unwrap_or("{}")).unwrap_or(Value::Null)
                })
        });
        (turn, tool)
    }

    async fn uncertain(&self) -> Vec<agentkit_store::ExecutionReceiptRecord> {
        self.service.list_uncertain_executions("s1").await.unwrap()
    }

    async fn resolve(
        &self,
        id: &str,
        outcome: ResolutionOutcome,
        result: Option<Value>,
    ) -> SessionResult<agentkit_store::ExecutionReceiptRecord> {
        self.service
            .resolve_uncertain_execution(
                "s1",
                id,
                outcome,
                result,
                Some("checked".into()),
                Some("op".into()),
            )
            .await
    }
}

#[tokio::test]
async fn erroring_body_blocks_a_new_run_until_not_started_resolution() {
    let f = fixture().await;
    f.run_turn("t1").await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    let stranded = f.uncertain().await;
    assert_eq!(stranded.len(), 1, "an Err body leaves the receipt admitted");
    assert_eq!(stranded[0].status, ExecutionReceiptStatus::Admitted);

    // A NEW run issuing the identical call is refused, not executed.
    let (_, refusal) = f.run_turn("t2").await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        refusal.unwrap()["data"]["errorCode"],
        json!("tool_result_uncertain")
    );

    let resolved = f
        .resolve(
            &stranded[0].execution_id,
            ResolutionOutcome::NotStarted,
            None,
        )
        .await
        .unwrap();
    assert_eq!(resolved.status, ExecutionReceiptStatus::Resolved);
    assert!(f.uncertain().await.is_empty());

    f.fail.store(false, Ordering::SeqCst);
    let (turn, tool) = f.run_turn("t3").await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 2, "the identical call runs");
    assert_eq!(tool.unwrap()["data"]["done"], json!(true));
    assert_eq!(
        f.store.get_task(&turn).await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}

#[tokio::test]
async fn acknowledged_resolution_also_unblocks() {
    let f = fixture().await;
    f.run_turn("t1").await;
    let id = f.uncertain().await[0].execution_id.clone();
    f.resolve(&id, ResolutionOutcome::Acknowledged, None)
        .await
        .unwrap();
    f.fail.store(false, Ordering::SeqCst);
    f.run_turn("t2").await;
    assert_eq!(f.hits.load(Ordering::SeqCst), 2);
}

/// The text of the tool message answering `call_id` in the newest request.
fn answer_to(f: &Fixture, call_id: &str) -> String {
    let requests = f.requests.lock().unwrap();
    let message = requests
        .iter()
        .rev()
        .flat_map(|request| request.messages.iter())
        .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(call_id))
        .expect("the stranded call is answered in history");
    message.content.as_str().unwrap().to_owned()
}

#[tokio::test]
async fn completed_resolution_reconciles_history_with_the_supplied_result() {
    let f = fixture().await;
    let id = f.seed_admitted("run-old", "seed-1", true).await;
    let record = f
        .resolve(&id, ResolutionOutcome::Completed, Some(json!("wrote it")))
        .await
        .unwrap();
    assert_eq!(record.status, ExecutionReceiptStatus::Completed);
    // Repeating the same resolution is idempotent.
    let again = f
        .resolve(&id, ResolutionOutcome::Completed, Some(json!("wrote it")))
        .await
        .unwrap();
    assert_eq!(again, record);
    // A different outcome after the fact is refused.
    let conflict = f
        .resolve(&id, ResolutionOutcome::NotStarted, None)
        .await
        .unwrap_err();
    assert_eq!(conflict.code, "receipt_not_uncertain");

    f.run_turn("t1").await;
    let answer = answer_to(&f, "seed-1");
    assert!(answer.contains("wrote it"), "{answer}");
    assert!(!answer.contains("tool_effect_uncertain"), "{answer}");
}

#[tokio::test]
async fn unresolved_and_not_started_receipts_never_reconcile_as_success() {
    let f = fixture().await;
    let id = f.seed_admitted("run-old", "seed-1", true).await;
    f.run_turn("t1").await;
    assert!(answer_to(&f, "seed-1").contains("tool_effect_uncertain"));

    f.resolve(&id, ResolutionOutcome::NotStarted, None)
        .await
        .unwrap();
    f.run_turn("t2").await;
    let answer: Value = serde_json::from_str(&answer_to(&f, "seed-1")).unwrap();
    assert_eq!(answer["ok"], json!(false));
    assert_eq!(
        answer["data"]["errorCode"],
        json!("tool_execution_resolved")
    );
}

#[tokio::test]
async fn wrong_session_and_invalid_payloads_are_refused() {
    let f = fixture().await;
    f.run_turn("t1").await;
    let id = f.uncertain().await[0].execution_id.clone();
    let other = f
        .service
        .resolve_uncertain_execution("s2", &id, ResolutionOutcome::NotStarted, None, None, None)
        .await
        .unwrap_err();
    assert_eq!(other.code, "not_found");
    let missing = f
        .service
        .resolve_uncertain_execution(
            "s1",
            "nope",
            ResolutionOutcome::NotStarted,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, "not_found");
    let no_result = f
        .resolve(&id, ResolutionOutcome::Completed, None)
        .await
        .unwrap_err();
    assert_eq!(no_result.code, "invalid_input");
    let extra = f
        .resolve(&id, ResolutionOutcome::Acknowledged, Some(json!("x")))
        .await
        .unwrap_err();
    assert_eq!(extra.code, "invalid_input");
    assert_eq!(f.uncertain().await.len(), 1, "nothing was resolved");
}

#[tokio::test]
async fn resolution_is_refused_while_the_receipts_run_is_active() {
    let f = fixture().await;
    let execution = f.seed_admitted("live", "call-x", false).await;
    let refused = f
        .resolve(&execution, ResolutionOutcome::NotStarted, None)
        .await
        .unwrap_err();
    assert_eq!(refused.code, RECEIPT_RUN_ACTIVE);
    assert_eq!(f.uncertain().await.len(), 1);
}
