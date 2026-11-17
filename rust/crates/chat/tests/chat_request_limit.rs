//! Chat request limit (hardening H1.2, decision D3): a chat turn on default
//! executor options may make 24 model requests. The old default of 4 also
//! capped every session profile that asked for more, since a profile limit
//! can only lower the executor's.

use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolContext,
    ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};
use agentkit_sessions::*;
use agentkit_store::{SqliteStore, TaskStatus};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

/// Calls `lookup` until it has seen `answer_after` results, then answers;
/// `None` never answers.
struct Scripted {
    requests: Arc<Mutex<usize>>,
    answer_after: Option<usize>,
}

#[async_trait]
impl ChatProvider for Scripted {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, input: ChatRequest, _: CancellationToken) -> ProviderStream {
        *self.requests.lock().unwrap() += 1;
        let results = input.messages.iter().filter(|m| m.role == "tool").count();
        let answer = self.answer_after.is_some_and(|after| results >= after);
        let calls = if answer {
            vec![]
        } else {
            vec![agentkit_core::ToolCall {
                id: format!("call-{results}"),
                name: "lookup".into(),
                arguments_json: "{}".into(),
            }]
        };
        Box::pin(futures::stream::iter(vec![Ok(ProviderEvent::Completed {
            content: if answer { "done" } else { "" }.into(),
            finish_reason: if answer { "stop" } else { "tool_calls" }.into(),
            tool_calls: calls,
        })]))
    }
}

struct Lookup(ToolDefinition);

#[async_trait]
impl Tool for Lookup {
    fn definition(&self) -> &ToolDefinition {
        &self.0
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"found": true})),
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

/// Runs one plain chat turn on default executor options; returns the
/// provider request count and the run's event types.
async fn turn(answer_after: Option<usize>) -> (usize, Vec<String>, TaskStatus) {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let requests = Arc::new(Mutex::new(0));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(Lookup(
            serde_json::from_value(json!({
                "name": "lookup", "version": "1", "effect": "read",
                "capability": "test.read", "description": "look something up",
                "inputSchema": {"type": "object"},
            }))
            .unwrap(),
        )))
        .unwrap();
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(ChatExecutor::new(
        Arc::new(Scripted {
            requests: requests.clone(),
            answer_after,
        }),
        Arc::new(tools),
    ));
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(CHAT_TURN_TASK_KIND.to_owned(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    host.submit_chat(SubmitChat {
        task_id: "t1".into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: json!("look it up"),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
    let events = store
        .events("t1", None, 10_000)
        .await
        .unwrap()
        .iter()
        .map(|event| {
            let kind = event.event["type"].as_str().unwrap_or_default().to_owned();
            match event.event["data"]["code"].as_str() {
                Some(code) => format!("{kind}:{code}"),
                None => kind,
            }
        })
        .collect();
    let status = store.get_task("t1").await.unwrap().unwrap().status;
    let count = *requests.lock().unwrap();
    (count, events, status)
}

#[test]
fn the_default_chat_request_limit_is_24() {
    assert_eq!(ChatExecutorOptions::default().max_tool_iterations, 24);
}

#[tokio::test]
async fn a_ten_step_turn_completes_on_default_options() {
    let (requests, events, status) = turn(Some(9)).await;
    assert_eq!(status, TaskStatus::Completed);
    assert_eq!(requests, 10, "nine lookups, then the answer");
    assert!(
        !events.iter().any(|e| e == "run.warning:max_iterations"),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_turn_that_never_answers_stops_after_24_requests() {
    let (requests, events, status) = turn(None).await;
    assert_eq!(status, TaskStatus::Completed);
    assert_eq!(requests, 24);
    assert!(
        events.iter().any(|e| e == "run.warning:max_iterations"),
        "{events:?}"
    );
}
