use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Provider;
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
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
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Write {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, _: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(CoreError::new(
            "unexpected_dispatch",
            "Tool must never be polled",
        ))
    }
}
#[derive(Debug)]
struct Guard {
    cancellation: CancellationToken,
    deny: bool,
}
#[async_trait]
impl DispatchGuard for Guard {
    async fn check(&self) -> Result<(), CoreError> {
        tokio::task::yield_now().await;
        if self.deny {
            Err(CoreError::new("lease_lost", "expired lease"))
        } else {
            self.cancellation.cancel();
            Ok(())
        }
    }
    fn fence(&self) -> DispatchFence {
        DispatchFence {
            task_id: "run".into(),
            attempt_id: "attempt".into(),
            fencing_token: 1,
        }
    }
}
#[tokio::test]
async fn denied_or_cancelled_dispatch_guard_prevents_first_tool_poll() {
    for deny in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let cancellation = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(Write { calls: calls.clone(), definition: serde_json::from_value(json!({"name":"write","version":"1","effect":"write","capability":"test","description":"Write","inputSchema":{"type":"object"}})).unwrap() })).unwrap();
        let run = run_chat(
            RunOptions {
                dispatch_guard: Some(Arc::new(Guard {
                    cancellation: cancellation.clone(),
                    deny,
                })),
                cancellation,
                ..Default::default()
            },
            Arc::new(Provider),
            Arc::new(registry),
        );
        let _: Vec<_> = run.events.collect().await;
        assert_eq!(run.result.await.unwrap().terminal, Terminal::Cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
