//! The run loop under a profile (T2.2): a step's reasoning rides on its
//! assistant message (TU-17) and calls the provider parsed out of reply text
//! are marked salvaged on their record (TU-21).

use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Script {
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    salvaged: bool,
}
#[async_trait]
impl ChatProvider for Script {
    fn id(&self) -> &str {
        "test"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let mut seen = self.requests.lock().unwrap();
        seen.push(request.messages);
        let events = if seen.len() == 1 {
            let call = ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                arguments_json: "{}".into(),
            };
            let mut events = vec![];
            if self.salvaged {
                events.push(Ok(ProviderEvent::Warning {
                    code: "tool_call_salvaged".into(),
                    message: "parsed from text".into(),
                }));
            }
            events.push(Ok(ProviderEvent::Event {
                kind: "run.message.completed".into(),
                data: json!({
                    "content": "",
                    "toolCallCount": 1,
                    "toolCalls": [call],
                    "reasoningContent": "I will call echo.",
                    "finishReason": "stop",
                }),
            }));
            events.push(Ok(ProviderEvent::ToolRequested(call)));
            events
        } else {
            vec![Ok(ProviderEvent::Completed {
                content: "done".into(),
                tool_calls: vec![],
                finish_reason: "stop".into(),
            })]
        };
        Box::pin(futures_util::stream::iter(events))
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
            data: Some(json!({"ok":true})),
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

async fn run(salvaged: bool) -> (Vec<Value>, Vec<Vec<ChatMessage>>) {
    let provider = Arc::new(Script {
        requests: Mutex::new(vec![]),
        salvaged,
    });
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(Echo(
            serde_json::from_value(json!({"name":"echo","version":"1","effect":"read",
                "capability":"test","description":"d","inputSchema":{"type":"object"}}))
            .unwrap(),
        )))
        .unwrap();
    let handle = run_chat(
        RunOptions {
            request: ChatRequest {
                model: "m".into(),
                ..Default::default()
            },
            tool_call_records: true,
            ..Default::default()
        },
        provider.clone(),
        Arc::new(registry),
    );
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect();
    let requests = provider.requests.lock().unwrap().clone();
    (events, requests)
}

fn record(events: &[Value]) -> &Value {
    events
        .iter()
        .find(|event| event["type"] == "run.tool.record")
        .map(|event| &event["data"])
        .unwrap()
}

#[tokio::test]
async fn the_next_request_carries_the_step_reasoning_on_its_assistant_message() {
    let (_, requests) = run(false).await;
    let assistant = requests[1]
        .iter()
        .find(|message| message.role == "assistant")
        .unwrap();
    assert_eq!(
        assistant.metadata.as_ref().unwrap()[REASONING_METADATA_KEY],
        "I will call echo."
    );
}

#[tokio::test]
async fn a_salvaged_turn_marks_its_calls_salvaged_and_others_stay_false() {
    let (events, _) = run(true).await;
    assert_eq!(record(&events)["salvaged"], true);
    let (events, _) = run(false).await;
    assert_eq!(record(&events)["salvaged"], false);
}
