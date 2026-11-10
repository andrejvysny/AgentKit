//! `run.message.completed.reasoningContent` is projected into the persisted
//! assistant message metadata (TS `turn/reasoning-projection.ts`).
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

struct Provider {
    steps: Mutex<Vec<Vec<ProviderEvent>>>,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let step = self.steps.lock().unwrap().remove(0);
        Box::pin(futures::stream::iter(step.into_iter().map(Ok)))
    }
}
struct Echo(ToolDefinition);
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({})),
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
fn reasoning(delta: &str) -> ProviderEvent {
    ProviderEvent::Event {
        kind: "run.message.reasoning-delta".into(),
        data: json!({"delta": delta}),
    }
}
fn completed(content: &str, calls: bool, reasoning: Option<&str>) -> ProviderEvent {
    let tool_calls = if calls {
        json!([{"id":"one","name":"echo","argumentsJson":"{}"}])
    } else {
        json!([])
    };
    let mut data = json!({
        "content": content,
        "toolCallCount": usize::from(calls),
        "toolCalls": tool_calls,
        "finishReason": if calls { "tool_calls" } else { "stop" },
    });
    if let Some(reasoning) = reasoning {
        data["reasoningContent"] = reasoning.into();
    }
    ProviderEvent::Event {
        kind: "run.message.completed".into(),
        data,
    }
}
async fn run(path: &std::path::Path, steps: Vec<Vec<ProviderEvent>>) -> SqliteStore {
    let store = SqliteStore::open(path, 64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo(serde_json::from_value(json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object"}})).unwrap()))).unwrap();
    let provider = Arc::new(Provider {
        steps: Mutex::new(steps),
    });
    let executor: Arc<dyn TaskExecutor<agentkit_store::SqliteStore>> =
        Arc::new(ChatExecutor::new(provider, Arc::new(tools)));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    host.submit_chat(SubmitChat {
        task_id: "run".into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: json!("question"),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
    store
}

#[tokio::test]
async fn reasoning_lands_in_metadata_across_steps_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("store.db");
    // Step 1 completes with a value that replaces its deltas; step 2 has only
    // deltas, which the TS projection keeps when the event carries no value.
    let store = run(
        &path,
        vec![
            vec![
                ProviderEvent::Started,
                reasoning("draft"),
                completed("", true, Some("Think ")),
            ],
            vec![
                ProviderEvent::Started,
                reasoning("harder"),
                completed("answer", false, None),
            ],
        ],
    )
    .await;
    drop(store);
    let store = SqliteStore::open(&path, 64).unwrap();
    let messages = store.messages("chat", true).await.unwrap();
    let call = messages.iter().find(|m| m.tool_calls.is_some()).unwrap();
    assert_eq!(call.metadata["internal"], true);
    assert_eq!(call.metadata["reasoningContent"], "Think ");
    let answer = messages
        .iter()
        .find(|m| m.metadata.get("placeholder").is_some())
        .unwrap();
    assert_eq!(answer.content, "answer");
    assert_eq!(answer.metadata["placeholder"], false);
    assert_eq!(answer.metadata["reasoningContent"], "Think harder");
}

#[tokio::test]
async fn absent_reasoning_leaves_metadata_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let store = run(
        &directory.path().join("store.db"),
        vec![vec![
            ProviderEvent::Started,
            completed("answer", false, None),
        ]],
    )
    .await;
    let messages = store.messages("chat", true).await.unwrap();
    let answer = messages
        .iter()
        .find(|m| m.metadata.get("placeholder").is_some())
        .unwrap();
    assert_eq!(answer.metadata, json!({"placeholder": false}));
}
