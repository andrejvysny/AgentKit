use agentkit_core::*;
use agentkit_host::{
    AgentHost, ChatExecutor, ChatExecutorOptions, ContextProvider, HostOptions, HostResult,
    SubmitChat, TaskExecutor,
};
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Context;
#[async_trait]
impl ContextProvider for Context {
    async fn resolve(&self, _: Value, _: CancellationToken) -> HostResult<Value> {
        Ok(agentkit_contracts::parse_javascript_json(r#"{"bindings":[{"id":"b","kind":"document","refId":"r","label":"Document","role":"primary","status":"active","metadata":{"nested":{"$serde_json::private::Number":"123"}}}]}"#).unwrap())
    }
}
struct Provider;
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let done = request
            .messages
            .iter()
            .any(|message| message.role == "tool");
        Box::pin(futures::stream::iter([Ok(ProviderEvent::Completed {
            content: if done { "answer" } else { "" }.into(),
            tool_calls: if done {
                vec![]
            } else {
                vec![ToolCall {
                    id: "call".into(),
                    name: "echo".into(),
                    arguments_json: "{}".into(),
                }]
            },
            finish_reason: if done { "stop" } else { "tool_calls" }.into(),
        })]))
    }
}
struct Echo {
    definition: ToolDefinition,
    bindings: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        *self.bindings.lock().unwrap() = context.bindings;
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
        })
    }
}
#[tokio::test]
async fn context_bindings_keep_private_number_object_keys_through_live_tool_callback() {
    let store = SqliteStore::memory(32).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let bindings = Arc::new(Mutex::new(vec![]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo { bindings: bindings.clone(), definition: serde_json::from_value(json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object"}})).unwrap() })).unwrap();
    let executor: Arc<dyn TaskExecutor> = Arc::new(
        ChatExecutor::new(Arc::new(Provider), Arc::new(tools)).with_options(ChatExecutorOptions {
            context: Some(Arc::new(Context)),
            ..Default::default()
        }),
    );
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
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    let bindings = bindings.lock().unwrap();
    assert_eq!(bindings.len(), 1);
    assert!(bindings[0]["metadata"]["nested"].is_object());
    assert_eq!(
        bindings[0]["metadata"]["nested"]["$serde_json::private::Number"],
        "123"
    );
}
