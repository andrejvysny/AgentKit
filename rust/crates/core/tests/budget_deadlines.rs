use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};

struct Clock(AtomicU64);
impl Runtime for Clock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    fn timestamp(&self) -> String {
        "2026-10-22T12:00:00.000Z".into()
    }
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", uuid::Uuid::new_v4())
    }
}
struct Provider {
    clock: Arc<Clock>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.0.fetch_add(60, Ordering::SeqCst);
        Box::pin(futures_util::stream::iter([Ok(ProviderEvent::Completed {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call".into(),
                name: "write".into(),
                arguments_json: "{}".into(),
            }],
            finish_reason: "tool_calls".into(),
        })]))
    }
}
struct Write {
    definition: ToolDefinition,
    clock: Arc<Clock>,
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Write {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.0.fetch_add(41, Ordering::SeqCst);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"applied":true})),
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
fn registry(clock: Arc<Clock>, calls: Arc<AtomicUsize>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Write { definition: serde_json::from_value(json!({"name":"write","version":"1","effect":"write","capability":"test","description":"Write","inputSchema":{"type":"object"}})).unwrap(), clock, calls })).unwrap();
    Arc::new(registry)
}
fn options(clock: Arc<Clock>, budget: Arc<ExecutionBudget>) -> RunOptions {
    RunOptions {
        request: ChatRequest {
            budget: Some(budget),
            ..Default::default()
        },
        runtime: clock,
        ..Default::default()
    }
}
fn assert_timeout_after_success(events: &[agentkit_contracts::AiRunEvent]) {
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type() == "run.tool.succeeded")
            .count(),
        1
    );
    let failure = serde_json::to_value(events.last().unwrap()).unwrap();
    assert_eq!(failure["data"]["errorCode"], "execution_timeout");
    assert_eq!(
        failure["data"]["errorMessage"],
        "execution_timeout: overall"
    );
}
#[tokio::test]
async fn provider_tool_and_reopened_budget_share_total_deadline_snapshot() {
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let limits = BudgetLimits {
        overall_ms: Some(100),
        tool_ms: Some(100),
        provider_requests: Some(3),
        tool_calls: Some(3),
        ..Default::default()
    };
    let budget = Arc::new(ExecutionBudget::new(limits.clone(), clock.clone(), None).unwrap());
    let initial = budget.snapshot();
    let provider = Arc::new(Provider {
        clock: clock.clone(),
        calls: provider_calls.clone(),
    });
    let run = run_chat(
        options(clock.clone(), budget.clone()),
        provider.clone(),
        registry(clock.clone(), tool_calls.clone()),
    );
    let events: Vec<_> = run.events.collect().await;
    assert_eq!(run.result.await.unwrap().terminal, Terminal::Failed);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
    assert_timeout_after_success(&events);
    let snapshot = budget.snapshot();
    assert_eq!(snapshot.started_at, initial.started_at);
    assert_eq!(snapshot.provider_requests, 1);
    assert_eq!(snapshot.tool_calls, 1);
    let restored = Arc::new(ExecutionBudget::new(limits, clock.clone(), Some(snapshot)).unwrap());
    assert_eq!(restored.remaining_ms(), Some(0));
    let run = run_chat(
        options(clock, restored),
        provider,
        Arc::new(ToolRegistry::new()),
    );
    let _: Vec<_> = run.events.collect().await;
    assert_eq!(run.result.await.unwrap().terminal, Terminal::Failed);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1);
}
