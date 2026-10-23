use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_host::*;
use agentkit_store::{SqliteStore, TaskStatus};
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
struct Provider {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    tool: bool,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        let count = self.requests.lock().unwrap().len();
        self.requests.lock().unwrap().push(input);
        let calls = if self.tool && count == 0 {
            vec![agentkit_core::ToolCall {
                id: "one".into(),
                name: "echo".into(),
                arguments_json: "{}".into(),
            }]
        } else {
            vec![]
        };
        Box::pin(futures::stream::iter(vec![
            Ok(ProviderEvent::Started),
            Ok(ProviderEvent::Completed {
                content: if calls.is_empty() {
                    "answer".into()
                } else {
                    String::new()
                },
                finish_reason: if calls.is_empty() {
                    "stop".into()
                } else {
                    "tool_calls".into()
                },
                tool_calls: calls,
            }),
        ]))
    }
}
struct Echo {
    definition: ToolDefinition,
    bindings: Arc<Mutex<Vec<Value>>>,
    store: SqliteStore,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        let events = self
            .store
            .events(&context.run_id, None, 1000)
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event["type"] == "run.tool.running")
        );
        let messages = self.store.messages("chat", false).await.unwrap();
        assert!(messages.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| calls[0]["id"] == "one")
        }));
        *self.bindings.lock().unwrap() = context.bindings;
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"echo":true})),
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
fn make_host(
    store: SqliteStore,
    provider: Arc<dyn ChatProvider>,
    tools: ToolRegistry,
    options: ChatExecutorOptions,
) -> AgentHost {
    let executor: Arc<dyn TaskExecutor> =
        Arc::new(ChatExecutor::new(provider, Arc::new(tools)).with_options(options));
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
    host.run_once().await.unwrap();
}
async fn store() -> SqliteStore {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    store
}
struct Context;
#[async_trait]
impl ContextProvider for Context {
    async fn resolve(&self, input: Value, _: CancellationToken) -> HostResult<Value> {
        assert_eq!(input["chatId"], "chat");
        Ok(
            json!({"systemPrompt":"System context","bindings":[{"id":"b","kind":"document","refId":"r","label":"Document","role":"primary","status":"active"}]}),
        )
    }
}
#[tokio::test]
async fn context_and_canonical_tools_are_durable_before_effect_then_replay_balanced() {
    let store = store().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let bindings = Arc::new(Mutex::new(vec![]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(Echo {store:store.clone(),bindings:bindings.clone(),definition:serde_json::from_value(json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object"}})).unwrap()})).unwrap();
    let host = make_host(
        store.clone(),
        Arc::new(Provider {
            requests: requests.clone(),
            tool: true,
        }),
        tools,
        ChatExecutorOptions {
            context: Some(Arc::new(Context)),
            ..Default::default()
        },
    );
    submit(&host, "run1", json!("question")).await;
    assert_eq!(
        store.get_task("run1").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(bindings.lock().unwrap()[0]["id"], "b");
    assert_eq!(
        requests.lock().unwrap()[0].messages[0].content,
        "System context"
    );
    submit(&host, "run2", json!("next")).await;
    let requests = requests.lock().unwrap();
    let messages = &requests[2].messages;
    assert_eq!(
        messages
            .iter()
            .map(|message| message.role.as_str())
            .collect::<Vec<_>>(),
        ["system", "user", "assistant", "tool", "assistant", "user"]
    );
    assert_eq!(messages[1].content, "question");
    assert_eq!(
        messages[2].tool_calls[0].id,
        messages[3].tool_call_id.as_deref().unwrap()
    );
    assert_eq!(messages[4].content, "answer");
}
struct Attachments {
    calls: AtomicUsize,
}
#[async_trait]
impl AttachmentResolver for Attachments {
    async fn resolve(
        &self,
        reference: &str,
        context: &AttachmentResolveContext,
        _: CancellationToken,
    ) -> HostResult<Option<ResolvedAttachment>> {
        assert_eq!(reference, "ref");
        assert_eq!(context.chat_id, "chat");
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ResolvedAttachment {
            media_type: "image/png".into(),
            base64: "aGVsbG8=".into(),
        }))
    }
}
#[tokio::test]
async fn attachment_cache_and_budgets_are_per_pass_without_rewriting_stored_refs() {
    let store = store().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let attachments = Arc::new(Attachments {
        calls: AtomicUsize::new(0),
    });
    let options = ChatExecutorOptions {
        attachments: Some(attachments.clone()),
        attachment_budgets: AttachmentBudgets {
            max_bytes_per_image: 100,
            max_total_bytes: 100,
            max_images: 1,
        },
        ..Default::default()
    };
    let host = make_host(
        store.clone(),
        Arc::new(Provider {
            requests: requests.clone(),
            tool: false,
        }),
        ToolRegistry::new(),
        options,
    );
    submit(&host,"images",json!([{"type":"text","text":"view"},{"type":"image","source":{"kind":"ref","ref":"ref"}},{"type":"image","source":{"kind":"ref","ref":"ref"}}])).await;
    assert_eq!(attachments.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        requests.lock().unwrap()[0].messages[0]
            .content
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .get_message("images_user")
            .await
            .unwrap()
            .unwrap()
            .content[1]["source"]["kind"],
        "ref"
    );
    assert!(
        store
            .events("images", None, 1000)
            .await
            .unwrap()
            .iter()
            .any(|event| event.event["data"]["code"] == "attachment_budget_exceeded")
    );
}
struct Denied;
#[async_trait]
impl UsageAuthorizer for Denied {
    async fn authorize(&self, _: &str, input: &Value) -> HostResult<()> {
        assert_eq!(input["providerId"], "test");
        Err(HostError::Execution("quota exhausted".into()))
    }
}
#[tokio::test]
async fn usage_denial_prevents_provider_work_and_settles_failed() {
    let store = store().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let host = make_host(
        store.clone(),
        Arc::new(Provider {
            requests: requests.clone(),
            tool: false,
        }),
        ToolRegistry::new(),
        ChatExecutorOptions {
            usage: Some(Arc::new(Denied)),
            ..Default::default()
        },
    );
    submit(&host, "denied", json!("question")).await;
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        store.get_task("denied").await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
}
struct SlowContext;
#[async_trait]
impl ContextProvider for SlowContext {
    async fn resolve(&self, _: Value, _: CancellationToken) -> HostResult<Value> {
        futures::future::pending().await
    }
}
#[tokio::test]
async fn stalled_context_degrades_under_deadline_and_emits_durable_warning() {
    let store = store().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let options = ChatExecutorOptions {
        context: Some(Arc::new(SlowContext)),
        hook_timeouts: ChatHookTimeouts {
            context_ms: 5,
            ..Default::default()
        },
        ..Default::default()
    };
    let host = make_host(
        store.clone(),
        Arc::new(Provider {
            requests: requests.clone(),
            tool: false,
        }),
        ToolRegistry::new(),
        options,
    );
    submit(&host, "context-timeout", json!("question")).await;
    assert_eq!(requests.lock().unwrap()[0].messages.len(), 1);
    assert!(
        store
            .events("context-timeout", None, 1000)
            .await
            .unwrap()
            .iter()
            .any(|event| event.event["data"]["code"] == "hook_timeout")
    );
    assert_eq!(
        store
            .get_task("context-timeout")
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Completed
    );
}

struct FrozenFactory {
    seen: Mutex<Vec<Value>>,
    current_generation: AtomicUsize,
    provider: Arc<dyn ChatProvider>,
}
#[async_trait]
impl ChatProviderFactory for FrozenFactory {
    async fn resolve(
        &self,
        snapshot: &Value,
        _: CancellationToken,
    ) -> HostResult<Arc<dyn ChatProvider>> {
        self.seen.lock().unwrap().push(snapshot.clone());
        if snapshot["connection"]["generation"].as_u64()
            != Some(self.current_generation.load(Ordering::SeqCst) as u64)
        {
            return Err(HostError::Execution("connection_changed".into()));
        }
        Ok(self.provider.clone())
    }
}
#[tokio::test]
async fn queued_provider_snapshot_never_changes_to_new_canonical_generation() {
    use agentkit_store::{MessageInput, SubmitTask};
    let store = store().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let factory = Arc::new(FrozenFactory {
        seen: Mutex::new(vec![]),
        current_generation: AtomicUsize::new(2),
        provider: Arc::new(Provider {
            requests: requests.clone(),
            tool: false,
        }),
    });
    let host = make_host(
        store.clone(),
        Arc::new(Provider {
            requests: requests.clone(),
            tool: false,
        }),
        ToolRegistry::new(),
        ChatExecutorOptions {
            provider_factory: Some(factory.clone()),
            ..Default::default()
        },
    );
    let snapshot = json!({"connection":{"id":"old","generation":1},"model":"model"});
    let task = SubmitTask {
        task_id: "frozen".into(),
        kind: "chat.turn".into(),
        scope_id: "chat".into(),
        payload: json!({"chatId":"chat","model":"model","assistantMessageId":"frozen_assistant","providerSnapshot":snapshot}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: true,
    };
    store
        .submit_turn(
            task,
            vec![
                MessageInput {
                    id: Some("frozen_user".into()),
                    chat_id: "chat".into(),
                    content: "question".into(),
                    ..Default::default()
                },
                MessageInput {
                    id: Some("frozen_assistant".into()),
                    chat_id: "chat".into(),
                    run_id: Some("frozen".into()),
                    role: "assistant".into(),
                    metadata: json!({"placeholder":true}),
                    ..Default::default()
                },
            ],
        )
        .await
        .unwrap();
    host.run_once().await.unwrap();
    assert_eq!(factory.seen.lock().unwrap()[0], snapshot);
    assert_eq!(
        store.get_task("frozen").await.unwrap().unwrap().status,
        TaskStatus::Failed
    );
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_submit_replay_rejects_changed_parent_identity() {
    use agentkit_store::MessageInput;
    let store = store().await;
    let requests = Arc::new(Mutex::new(vec![]));
    let first = store
        .append_message(
            MessageInput {
                chat_id: "chat".into(),
                content: "root".into(),
                ..Default::default()
            },
            None,
            0,
        )
        .await
        .unwrap();
    let second = store
        .append_message(
            MessageInput {
                chat_id: "chat".into(),
                content: "branch".into(),
                ..Default::default()
            },
            None,
            1,
        )
        .await
        .unwrap();
    let host = make_host(
        store,
        Arc::new(Provider {
            requests,
            tool: false,
        }),
        ToolRegistry::new(),
        ChatExecutorOptions::default(),
    );
    host.submit_chat(SubmitChat {
        task_id: "same".into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: "question".into(),
        parent_message_id: Some(first.id),
    })
    .await
    .unwrap();
    assert!(
        host.submit_chat(SubmitChat {
            task_id: "same".into(),
            chat_id: "chat".into(),
            model: "model".into(),
            content: "question".into(),
            parent_message_id: Some(second.id)
        })
        .await
        .is_err()
    );
}
