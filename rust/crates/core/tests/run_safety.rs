use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
struct Calls;
#[async_trait]
impl ChatProvider for Calls {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        Box::pin(futures_util::stream::iter(vec![
            Ok(ProviderEvent::Started),
            Ok(ProviderEvent::Completed {
                content: String::new(),
                tool_calls: (0..3)
                    .map(|index| ToolCall {
                        id: format!("call{index}"),
                        name: "write".into(),
                        arguments_json: "{}".into(),
                    })
                    .collect(),
                finish_reason: "tool_calls".into(),
            }),
        ]))
    }
}
struct Write {
    definition: ToolDefinition,
    count: Arc<AtomicUsize>,
    cancel: bool,
}
#[async_trait]
impl Tool for Write {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        if self.cancel {
            context.cancellation.cancel();
        }
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
fn registry(count: Arc<AtomicUsize>, cancel: bool) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Write {count,cancel,definition:serde_json::from_value(json!({"name":"write","version":"1","effect":"write","capability":"test","description":"Write","inputSchema":{"type":"object"}})).unwrap()})).unwrap();
    Arc::new(registry)
}
struct RejectRunning;
#[async_trait]
impl RunEventSink for RejectRunning {
    async fn append(&self, event: &agentkit_contracts::AiRunEvent) -> Result<(), CoreError> {
        if event.event_type() == "run.tool.running" {
            Err(CoreError::new("store_error", "durable append failed"))
        } else {
            Ok(())
        }
    }
}
#[tokio::test]
async fn failed_durable_append_stops_before_tool_side_effect() {
    let count = Arc::new(AtomicUsize::new(0));
    let handle = run_chat(
        RunOptions {
            event_sink: Some(Arc::new(RejectRunning)),
            ..Default::default()
        },
        Arc::new(Calls),
        registry(count.clone(), false),
    );
    let _: Vec<_> = handle.events.collect().await;
    assert_eq!(handle.result.await.unwrap().terminal, Terminal::Failed);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
struct CancelAfterSuccess {
    token: CancellationToken,
}
#[async_trait]
impl RunEventSink for CancelAfterSuccess {
    async fn append(&self, event: &agentkit_contracts::AiRunEvent) -> Result<(), CoreError> {
        if event.event_type() == "run.tool.succeeded" {
            self.token.cancel();
        }
        Ok(())
    }
}
#[tokio::test]
async fn cancellation_during_persisted_success_keeps_result_and_stops_next_tool() {
    let count = Arc::new(AtomicUsize::new(0));
    let token = CancellationToken::new();
    let options = RunOptions {
        cancellation: token.clone(),
        event_sink: Some(Arc::new(CancelAfterSuccess { token })),
        ..Default::default()
    };
    let handle = run_chat(options, Arc::new(Calls), registry(count.clone(), false));
    let events: Vec<_> = handle.events.collect().await;
    let result = handle.result.await.unwrap();
    assert_eq!(result.terminal, Terminal::Cancelled);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type() == "run.tool.succeeded")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type() == "run.tool.failed")
            .count(),
        2
    );
    assert_eq!(result.appended_messages.len(), 4);
}
#[tokio::test]
async fn noncooperative_provider_is_bounded_by_first_byte_deadline() {
    struct Hanging;
    #[async_trait]
    impl ChatProvider for Hanging {
        fn id(&self) -> &str {
            "hang"
        }
        fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
            Box::pin(futures_util::stream::pending())
        }
    }
    let budget = ExecutionBudget::new(
        BudgetLimits {
            first_byte_ms: Some(5),
            ..Default::default()
        },
        Arc::new(SystemRuntime),
        None,
    )
    .unwrap();
    let options = RunOptions {
        request: ChatRequest {
            budget: Some(Arc::new(budget)),
            ..Default::default()
        },
        ..Default::default()
    };
    let handle = run_chat(options, Arc::new(Hanging), Arc::new(ToolRegistry::new()));
    let events: Vec<_> = handle.events.collect().await;
    assert_eq!(
        serde_json::to_value(events.last().unwrap()).unwrap()["data"]["errorCode"],
        "execution_timeout"
    );
    assert_eq!(handle.result.await.unwrap().terminal, Terminal::Failed);
}

struct CancelRunning {
    token: CancellationToken,
}
#[async_trait]
impl RunEventSink for CancelRunning {
    async fn append(&self, event: &agentkit_contracts::AiRunEvent) -> Result<(), CoreError> {
        if event.event_type() == "run.tool.running" {
            self.token.cancel();
        }
        Ok(())
    }
}
#[tokio::test]
async fn cancellation_during_running_event_persistence_prevents_tool_dispatch() {
    let count = Arc::new(AtomicUsize::new(0));
    let token = CancellationToken::new();
    let options = RunOptions {
        cancellation: token.clone(),
        event_sink: Some(Arc::new(CancelRunning { token })),
        ..Default::default()
    };
    let handle = run_chat(options, Arc::new(Calls), registry(count.clone(), false));
    let _: Vec<_> = handle.events.collect().await;
    assert_eq!(handle.result.await.unwrap().terminal, Terminal::Cancelled);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}
struct FailedCheckpoint;
#[async_trait]
impl BudgetCheckpoint for FailedCheckpoint {
    async fn checkpoint(&self, _: BudgetSnapshot) -> Result<(), CoreError> {
        Err(CoreError::new("checkpoint_failed", "checkpoint failed"))
    }
}
#[tokio::test]
async fn checkpoint_failure_prevents_provider_and_tool_dispatch() {
    struct CountProvider(Arc<AtomicUsize>);
    #[async_trait]
    impl ChatProvider for CountProvider {
        fn id(&self) -> &str {
            "test"
        }
        fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(futures_util::stream::empty())
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let budget = ExecutionBudget::new(BudgetLimits::default(), Arc::new(SystemRuntime), None)
        .unwrap()
        .with_checkpoint(Arc::new(FailedCheckpoint));
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                budget: Some(Arc::new(budget)),
                ..Default::default()
            },
            ..Default::default()
        },
        Arc::new(CountProvider(calls.clone())),
        Arc::new(ToolRegistry::new()),
    );
    let _: Vec<_> = handle.events.collect().await;
    assert_eq!(handle.result.await.unwrap().terminal, Terminal::Failed);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

struct BillingProvider;
#[async_trait]
impl ChatProvider for BillingProvider {
    fn supports_usage_cleanup(&self) -> bool {
        true
    }
    fn id(&self) -> &str {
        "billing"
    }
    fn stream_chat(&self, _: ChatRequest, cancellation: CancellationToken) -> ProviderStream {
        Box::pin(async_stream::try_stream! {
            yield ProviderEvent::Started;yield ProviderEvent::Delta("partial".into());
            cancellation.cancelled().await;
            yield ProviderEvent::Usage(json!({"callId":"billed","attempt":1,"step":0,"model":"m","source":"stream","finalForCall":false,"totalTokens":7}));
            Err(CoreError::new("cancelled","aborted"))?;
        })
    }
}
struct CancelDelta(CancellationToken);
#[async_trait]
impl RunEventSink for CancelDelta {
    async fn append(&self, event: &agentkit_contracts::AiRunEvent) -> Result<(), CoreError> {
        if event.event_type() == "run.message.delta" {
            self.0.cancel();
        }
        Ok(())
    }
}
#[tokio::test]
async fn cancellation_and_timeout_preserve_already_observed_usage_without_waiting_for_cleanup() {
    for cancelled in [true, false] {
        let token = CancellationToken::new();
        let budget = ExecutionBudget::new(
            BudgetLimits {
                stream_idle_ms: Some(5),
                ..Default::default()
            },
            Arc::new(SystemRuntime),
            None,
        )
        .unwrap();
        let options = RunOptions {
            cancellation: token.clone(),
            event_sink: if cancelled {
                Some(Arc::new(CancelDelta(token)) as Arc<dyn RunEventSink>)
            } else {
                None
            },
            request: ChatRequest {
                budget: Some(Arc::new(budget)),
                ..Default::default()
            },
            ..Default::default()
        };
        let handle = run_chat(
            options,
            Arc::new(BillingProvider),
            Arc::new(ToolRegistry::new()),
        );
        let events = handle.events.collect::<Vec<_>>().await;
        let result = handle.result.await.unwrap();
        assert!(events.iter().any(|event| event.event_type() == "run.usage"));
        assert_eq!(
            result.terminal,
            if cancelled {
                Terminal::Cancelled
            } else {
                Terminal::Failed
            }
        );
        assert!(result.appended_messages.is_empty());
    }
}

struct FailedToolCheckpoint;
#[async_trait]
impl BudgetCheckpoint for FailedToolCheckpoint {
    async fn checkpoint(&self, state: BudgetSnapshot) -> Result<(), CoreError> {
        if state.tool_calls > 0 {
            Err(CoreError::new("checkpoint_failed", "checkpoint failed"))
        } else {
            Ok(())
        }
    }
}
#[tokio::test]
async fn failed_tool_reservation_never_dispatches_tool_or_next_call() {
    let count = Arc::new(AtomicUsize::new(0));
    let budget = ExecutionBudget::new(BudgetLimits::default(), Arc::new(SystemRuntime), None)
        .unwrap()
        .with_checkpoint(Arc::new(FailedToolCheckpoint));
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                budget: Some(Arc::new(budget)),
                ..Default::default()
            },
            ..Default::default()
        },
        Arc::new(Calls),
        registry(count.clone(), false),
    );
    let events = handle.events.collect::<Vec<_>>().await;
    assert_eq!(handle.result.await.unwrap().terminal, Terminal::Failed);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type() == "run.tool.running")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type() == "run.tool.failed")
            .count(),
        3
    );
}
