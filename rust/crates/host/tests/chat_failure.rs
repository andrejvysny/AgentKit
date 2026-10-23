use agentkit_core::*;
use agentkit_host::{AgentHost, ChatExecutor, HostOptions, SubmitChat, TaskExecutor};
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct FailingProvider(AtomicUsize);
#[async_trait]
impl ChatProvider for FailingProvider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(futures::stream::iter([Err(CoreError::new(
            "provider_error",
            "the provider said no",
        ))]))
    }
}
#[tokio::test]
async fn failed_chat_keeps_provider_message_and_code_without_replaying() {
    let store = SqliteStore::memory(32).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let provider = Arc::new(FailingProvider(AtomicUsize::new(0)));
    let executor: Arc<dyn TaskExecutor> = Arc::new(ChatExecutor::new(
        provider.clone(),
        Arc::new(ToolRegistry::new()),
    ));
    let host = AgentHost::new(
        store.clone(),
        std::collections::HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(agentkit_host::SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    host.submit_chat(SubmitChat {
        task_id: "run".into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: "question".into(),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
    let task = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(task.error.unwrap().contains("the provider said no"));
    let events = store.events("run", None, 1000).await.unwrap();
    let failures: Vec<_> = events
        .iter()
        .filter(|event| event.event["type"] == "run.failed")
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].event["data"]["errorCode"], "provider_error");
    assert_eq!(
        failures[0].event["data"]["errorMessage"],
        "the provider said no"
    );
    assert!(!host.run_once().await.unwrap());
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
}
