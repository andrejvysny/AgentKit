use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Provider {
    turns: AtomicUsize,
}
#[async_trait]
impl ChatProvider for Provider {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, _: ChatRequest, _: CancellationToken) -> ProviderStream {
        let call = |id: &str, name: &str, args: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments_json: args.into(),
        };
        let event = if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
            ProviderEvent::Completed {
                content: String::new(),
                tool_calls: vec![
                    call("c1", "echo", r#"{"text":"secret-argument"}"#),
                    call("c2", "missing", "{}"),
                    call("c3", "echo", "{not json"),
                    call("c4", "echo", "{}"),
                    call("c5", "boom", "{}"),
                ],
                finish_reason: "tool_calls".into(),
            }
        } else {
            ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            }
        };
        Box::pin(futures_util::stream::iter([Ok(event)]))
    }
}
struct Fixed {
    definition: ToolDefinition,
    fail: bool,
}
#[async_trait]
impl Tool for Fixed {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        if self.fail {
            Err(CoreError::new("exec_failed", "tool broke"))
        } else {
            Ok(ToolResult {
                ok: true,
                data: Some(json!({"echo":"secret-result"})),
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
}
fn registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    let echo = json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}}}});
    let boom = json!({"name":"boom","version":"1","effect":"read","capability":"test","description":"Boom","inputSchema":{"type":"object"}});
    for (definition, fail) in [(echo, false), (boom, true)] {
        let definition = serde_json::from_value(definition).unwrap();
        registry
            .register(Arc::new(Fixed { definition, fail }))
            .unwrap();
    }
    Arc::new(registry)
}
async fn events(records: bool) -> Vec<Value> {
    let run = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "test-model".into(),
                ..Default::default()
            },
            tool_call_records: records,
            ..Default::default()
        },
        Arc::new(Provider {
            turns: AtomicUsize::new(0),
        }),
        registry(),
    );
    let events: Vec<_> = run.events.collect().await;
    events
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect()
}

#[tokio::test]
async fn every_tool_call_produces_one_record_with_an_outcome_class() {
    let events = events(true).await;
    let records: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "run.tool.record")
        .collect();
    let classes: Vec<(&str, &str)> = records
        .iter()
        .map(|r| {
            (
                r["data"]["toolCallId"].as_str().unwrap(),
                r["data"]["outcomeClass"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        classes,
        [
            ("c1", "ok"),
            ("c2", "unknown_tool"),
            ("c3", "invalid_json"),
            ("c4", "invalid_structure"),
            ("c5", "tool_error"),
        ]
    );
    assert_eq!(records[0]["data"]["model"], "test-model");
    assert_eq!(records[4]["data"]["failureCode"], "exec_failed");
    assert!(records[0]["data"]["durationMs"].is_number());
    assert!(records[0]["data"]["argumentBytes"].as_f64().unwrap() > 0.0);
    assert!(records[0]["data"]["resultBytes"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn records_carry_no_argument_or_result_content() {
    let text = serde_json::to_string(
        &events(true)
            .await
            .into_iter()
            .filter(|event| event["type"] == "run.tool.record")
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!text.contains("secret-argument") && !text.contains("secret-result"));
}

#[tokio::test]
async fn records_are_off_by_default() {
    assert!(
        events(false)
            .await
            .iter()
            .all(|event| event["type"] != "run.tool.record")
    );
}
