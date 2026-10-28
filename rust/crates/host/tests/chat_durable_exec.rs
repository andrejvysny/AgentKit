//! Durable execution ownership across crash boundaries (ADR 0020):
//! a completed receipt replays, an admitted-without-result receipt is
//! uncertain (never silently re-executed), and persisted unanswered calls
//! reconcile against receipts instead of an assumed absence.
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolLimits, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{
    ClaimInput, ExecutionReceiptAdmission, ExecutionReceiptStatus, MessageInput, SettleTask,
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

struct Scripted {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    /// Per-pass tool calls: (id, name, arguments_json). Missing pass = plain answer.
    passes: Vec<Vec<(String, String, String)>>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let count = self.requests.lock().unwrap().len();
        self.requests.lock().unwrap().push(input);
        let calls = self
            .passes
            .get(count)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(id, name, arguments_json)| agentkit_core::ToolCall {
                id,
                name,
                arguments_json,
            })
            .collect::<Vec<_>>();
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderEvent::Started),
            Ok(ProviderEvent::Completed {
                content: if calls.is_empty() {
                    "answer".into()
                } else {
                    String::new()
                },
                finish_reason: if calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                }
                .into(),
                tool_calls: calls,
            }),
        ]))
    }
}

struct Mutate {
    definition: ToolDefinition,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for Mutate {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        let count = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"executions": count})),
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

fn mutate_tool(executions: Arc<AtomicUsize>) -> Arc<Mutate> {
    Arc::new(Mutate {
        definition: serde_json::from_value(json!({
            "name": "mutate",
            "version": "1",
            "effect": "write",
            "capability": "test",
            "description": "Mutates",
            "inputSchema": {"type": "object"},
        }))
        .unwrap(),
        executions,
    })
}

async fn chat_store() -> SqliteStore {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    store
}

fn host_with(store: SqliteStore, provider: Arc<dyn ChatProvider>, tool: Arc<Mutate>) -> AgentHost {
    let mut tools = ToolRegistry::new();
    tools.register(tool).unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(provider, Arc::new(tools)).with_options(ChatExecutorOptions::default()),
    );
    AgentHost::new(
        store,
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap()
}

async fn submit(host: &AgentHost, id: &str, content: Value) {
    host.submit_chat(SubmitChat {
        task_id: id.into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content,
        parent_message_id: None,
    })
    .await
    .unwrap();
}

/// Persisted state left behind by a run that died: an assistant message
/// declaring `calls`, plus receipts admitted under `run_id` (with `done`
/// carrying a recorded result). Then the run is settled `failed` so the
/// host never re-executes it — the receipts are pure crash residue.
async fn seed_crashed_run(
    host: &AgentHost,
    store: &SqliteStore,
    run_id: &str,
    calls: &[&str],
    admitted: &[&str],
    completed: &[(&str, Value)],
) {
    submit(host, run_id, json!("work")).await;
    let claimed = store
        .claim(ClaimInput {
            owner_id: "crash".into(),
            now: now_ms(),
            ttl_ms: 600_000,
            kinds: vec!["chat.turn".into()],
            busy_scopes: vec![],
            max_attempts: 5,
        })
        .await
        .unwrap()
        .expect("run is queued");
    assert_eq!(claimed.task.task_id, run_id);
    let fence = claimed.lease.fence();
    // Admit sequentially, completing before admitting the next: the store's
    // pending-generation gate (by design) keeps at most one uncertain
    // instance per identical operation.
    for call in admitted {
        store
            .admit_execution(
                fence.clone(),
                ExecutionReceiptAdmission {
                    execution_id: agentkit_host::execution_id(run_id, call, None),
                    run_id: run_id.into(),
                    chat_id: "chat".into(),
                    tool_call_id: (*call).into(),
                    tool_name: "mutate".into(),
                    args_sha256: agentkit_host::args_sha256(&json!({})),
                    context_sha256: "0".repeat(64),
                    attempt_id: "1".into(),
                    approval_request_id: None,
                    approval_generation: None,
                    now: now_ms(),
                },
            )
            .await
            .unwrap();
        if let Some((_, result)) = completed.iter().find(|(done, _)| done == call) {
            store
                .complete_execution(
                    fence.clone(),
                    agentkit_host::execution_id(run_id, call, None),
                    result.clone(),
                    now_ms(),
                )
                .await
                .unwrap();
        }
    }
    store
        .append_message(
            MessageInput {
                id: Some(format!("{run_id}-assistant")),
                chat_id: "chat".into(),
                run_id: Some(run_id.into()),
                role: "assistant".into(),
                content: json!("working"),
                parent_message_id: None,
                tool_call_id: None,
                tool_calls: Some(
                    calls
                        .iter()
                        .map(|call| json!({"id": call, "name": "mutate", "argumentsJson": "{}"}))
                        .collect(),
                ),
                model_result_json: None,
                metadata: json!({}),
                activate: true,
            },
            None,
            now_ms(),
        )
        .await
        .unwrap();
    // The executor binds every run it starts; a crashed run that got past
    // the bind must stay visible to the cross-run (per-chat) receipt scan.
    store
        .bind_run(
            run_id,
            "chat",
            json!({
                "providerId": "test",
                "protocol": "responses",
                "connectionId": "c1",
                "generation": 1,
                "chatId": "chat",
                "branchId": "b1",
                "model": "model",
            }),
            fence.clone(),
            now_ms(),
        )
        .await
        .unwrap();
    store
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
}

fn recorded_result(executions: u64) -> Value {
    serde_json::to_value(ToolResult {
        ok: true,
        data: Some(json!({"executions": executions})),
        sources: vec![],
        warnings: vec![],
        truncated: false,
        limits: ToolLimits::default(),
        model_data: None,
        summary: None,
        status: None,
    })
    .unwrap()
}

fn tool_message(request: &ChatRequest, call_id: &str) -> Value {
    let message = request
        .messages
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some(call_id))
        .unwrap_or_else(|| panic!("no tool message for {call_id}"));
    match &message.content {
        Value::String(text) => serde_json::from_str(text).unwrap(),
        value => value.clone(),
    }
}

#[tokio::test]
async fn write_tool_carries_a_completed_receipt_after_execution() {
    let store = chat_store().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests,
            passes: vec![vec![("one".into(), "mutate".into(), "{}".into())]],
        }),
        mutate_tool(executions.clone()),
    );
    submit(&host, "run1", json!("go")).await;
    host.run_once().await.unwrap();
    assert_eq!(
        store.get_task("run1").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let receipts = store.list_executions("run1".into()).await.unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].tool_call_id, "one");
    assert_eq!(receipts[0].tool_name, "mutate");
    assert_eq!(receipts[0].status, ExecutionReceiptStatus::Completed);
    assert_eq!(receipts[0].run_id, "run1");
    assert_eq!(receipts[0].result.as_ref().unwrap()["ok"], json!(true));
}

#[tokio::test]
async fn repeated_call_id_replays_the_recorded_result_without_reexecuting() {
    let store = chat_store().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![
                vec![("one".into(), "mutate".into(), "{}".into())],
                // Provider (mis)behaviour: the same call id again.
                vec![("one".into(), "mutate".into(), "{}".into())],
            ],
        }),
        mutate_tool(executions.clone()),
    );
    submit(&host, "run1", json!("go")).await;
    host.run_once().await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    // One admission for the call; the second serving was a replay.
    assert_eq!(store.list_executions("run1".into()).await.unwrap().len(), 1);
    let requests = requests.lock().unwrap();
    let replay = tool_message(&requests[2], "one");
    assert_eq!(replay["data"]["executions"], json!(1));
}

#[tokio::test]
async fn admitted_without_result_refuses_reissue_across_runs() {
    let store = chat_store().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![vec![("one".into(), "mutate".into(), "{}".into())]],
        }),
        mutate_tool(executions.clone()),
    );
    // Crash residue: the identical operation was admitted under run0 and
    // its outcome is unknown.
    seed_crashed_run(&host, &store, "run0", &["seed"], &["seed"], &[]).await;
    submit(&host, "run1", json!("go")).await;
    host.run_once().await.unwrap();
    // The body must not run again; the refusal is delivered as a tool result.
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(
        store
            .list_executions("run1".into())
            .await
            .unwrap()
            .is_empty()
    );
    let refusal = {
        let requests = requests.lock().unwrap();
        tool_message(&requests[1], "one")
    };
    assert_eq!(refusal["ok"], json!(false));
    assert_eq!(refusal["data"]["errorCode"], json!("tool_result_uncertain"));
    assert_eq!(refusal["data"]["pendingRunId"], json!("run0"));
    assert_eq!(
        store.get_task("run1").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}

#[tokio::test]
async fn persisted_unanswered_calls_reconcile_against_receipts() {
    let store = chat_store().await;
    let executions = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let host = host_with(
        store.clone(),
        Arc::new(Scripted {
            requests: requests.clone(),
            passes: vec![],
        }),
        mutate_tool(executions.clone()),
    );
    // run0 declared three calls and died: one completed, one admitted
    // (uncertain), one never admitted (no receipt at all).
    seed_crashed_run(
        &host,
        &store,
        "run0",
        &["done", "hang", "lost"],
        &["done", "hang"],
        &[("done", recorded_result(7))],
    )
    .await;
    submit(&host, "run1", json!("continue")).await;
    host.run_once().await.unwrap();
    let requests = requests.lock().unwrap();
    let first = &requests[0];
    // A completed receipt replays its recorded model result.
    assert_eq!(tool_message(first, "done")["data"]["executions"], json!(7));
    // An admitted receipt reads as uncertain, never as absence.
    assert_eq!(
        tool_message(first, "hang")["data"]["errorCode"],
        json!("tool_effect_uncertain")
    );
    // Without any receipt the legacy synthetic-failure reconciliation applies.
    assert_eq!(
        tool_message(first, "lost")["data"]["errorCode"],
        json!("tool_result_missing")
    );
    // Reconciliation executed nothing.
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}
