//! One `run.tool.record` per tool call on the production chat path (TU-35):
//! a clean call, an aliased call and a failing call, with the identity the
//! executor knows (provider, lane, toolset).
use agentkit_chat::*;
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, ProviderEvent, ProviderStream, Tool, ToolCall,
    ToolContext, ToolDefinition, ToolRegistry, ToolResult,
};
use agentkit_engine::{SystemRuntime, TaskExecutor};
use agentkit_store::SqliteStore;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

struct Script {
    asked: Mutex<bool>,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let first = !std::mem::replace(&mut *self.asked.lock().unwrap(), true);
        let call = |id: &str, name: &str, args: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments_json: args.into(),
        };
        let event = if first {
            ProviderEvent::Completed {
                content: String::new(),
                finish_reason: "tool_calls".into(),
                tool_calls: vec![
                    call("c1", "edit_file", r#"{"file_path":"a.txt"}"#),
                    call("c2", "edit_file", r#"{"path":"b.txt"}"#),
                    call("c3", "no_such_tool", "{}"),
                ],
            }
        } else {
            ProviderEvent::Completed {
                content: "done".into(),
                finish_reason: "stop".into(),
                tool_calls: vec![],
            }
        };
        Box::pin(futures::stream::iter(vec![Ok(event)]))
    }
}

struct Edit {
    definition: ToolDefinition,
}
#[async_trait]
impl Tool for Edit {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"edited":true})),
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

async fn records(options: AgentExecutorOptions) -> Vec<Value> {
    let store = SqliteStore::memory(64).unwrap();
    store.create_chat("chat", None, json!({}), 0).await.unwrap();
    let mut tools = ToolRegistry::new();
    let definition =
        serde_json::from_value(json!({"name":"edit_file","version":"1","effect":"read",
        "capability":"test","description":"Edit","inputSchema":{"type":"object",
        "required":["file_path"],"properties":{"file_path":{"type":"string"}}}}))
        .unwrap();
    tools.register(Arc::new(Edit { definition })).unwrap();
    let executor: Arc<dyn TaskExecutor<SqliteStore>> = Arc::new(
        AgentExecutor::new(
            Arc::new(Script {
                asked: Mutex::new(false),
            }),
            Arc::new(tools),
        )
        .with_options(options),
    );
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("chat.turn".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    host.submit_chat(SubmitChat {
        task_id: "run1".into(),
        chat_id: "chat".into(),
        model: "model".into(),
        content: json!("edit"),
        parent_message_id: None,
    })
    .await
    .unwrap();
    host.run_once().await.unwrap();
    store
        .events("run1", None, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.event)
        .filter(|event| event["type"] == "run.tool.record")
        .map(|event| event["data"].clone())
        .collect()
}

#[tokio::test]
async fn chat_executor_emits_one_record_per_call_by_default() {
    let records = records(AgentExecutorOptions::default()).await;
    let ids: Vec<&str> = records
        .iter()
        .map(|r| r["toolCallId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    for record in &records {
        assert_eq!(record["model"], "model");
        assert_eq!(record["provider"], "test");
        assert_eq!(record["lane"], "chat_completions");
    }
    assert_eq!(records[0]["outcomeClass"], "ok");
    assert!(records[0]["toolset"].is_string());
    assert_eq!(records[0]["aliasesUsed"], json!([]));
    assert_eq!(records[1]["outcomeClass"], "ok");
    assert!(
        !records[1]["aliasesUsed"].as_array().unwrap().is_empty(),
        "the aliased call is never silent: {}",
        records[1]
    );
    assert_eq!(records[2]["outcomeClass"], "unknown_tool");
    assert!(records[2]["failureCode"].is_string());
}

#[tokio::test]
async fn records_can_be_turned_off() {
    let records = records(AgentExecutorOptions {
        tool_call_records: false,
        ..Default::default()
    })
    .await;
    assert!(records.is_empty());
}
