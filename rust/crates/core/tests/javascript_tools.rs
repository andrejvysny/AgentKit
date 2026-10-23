use agentkit_core::*;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

struct OverflowProvider;
#[async_trait]
impl ChatProvider for OverflowProvider {
    fn id(&self) -> &str {
        "overflow"
    }
    fn stream_chat(&self, request: ChatRequest, _: CancellationToken) -> ProviderStream {
        let done = request
            .messages
            .iter()
            .any(|message| message.role == "tool");
        Box::pin(futures_util::stream::iter([Ok(ProviderEvent::Completed {
            content: if done { "done" } else { "" }.into(),
            tool_calls: if done {
                vec![]
            } else {
                vec![ToolCall {
                    id: "call".into(),
                    name: "echo".into(),
                    arguments_json: r#"{"positive":1e400,"negative":-1e400,"nested":[1e400],"rounded":9007199254740993,"zero":-0}"#.into(),
                }]
            },
            finish_reason: if done { "stop" } else { "tool_calls" }.into(),
        })]))
    }
}
struct Echo {
    definition: ToolDefinition,
    inputs: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.inputs.lock().unwrap().push(input.clone());
        Ok(ToolResult {
            ok: true,
            data: Some(input),
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
async fn overflowing_numbers_reach_tool_unchanged_then_stringify_as_null() {
    let inputs = Arc::new(Mutex::new(vec![]));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Echo {
        definition: serde_json::from_value(json!({"name":"echo","version":"1","effect":"read","capability":"test","description":"Echo","inputSchema":{"type":"object","properties":{"positive":{"type":"number"},"negative":{"type":"number"},"nested":{"type":"array","items":{"type":"number"}}}}})).unwrap(),
        inputs: inputs.clone(),
    })).unwrap();
    let run = run_chat(
        RunOptions::default(),
        Arc::new(OverflowProvider),
        Arc::new(registry),
    );
    let events: Vec<_> = run.events.collect().await;
    assert_eq!(run.result.await.unwrap().terminal, Terminal::Completed);
    let inputs = inputs.lock().unwrap();
    assert_eq!(inputs.len(), 1);
    assert_callback_input(&inputs[0]);
    assert_serialized_output(&events);
}
fn assert_callback_input(input: &Value) {
    assert!(input["positive"].is_number());
    assert_eq!(
        agentkit_contracts::javascript_number(&input["positive"]),
        Some(f64::INFINITY)
    );
    assert_eq!(
        agentkit_contracts::javascript_number(&input["negative"]),
        Some(f64::NEG_INFINITY)
    );
    assert_eq!(
        agentkit_contracts::javascript_number(&input["nested"][0]),
        Some(f64::INFINITY)
    );
    assert_eq!(
        agentkit_contracts::javascript_number(&input["rounded"]),
        Some(9007199254740992.0)
    );
    assert!(agentkit_contracts::javascript_number(&input["zero"])
        .unwrap()
        .is_sign_negative());
}
fn assert_serialized_output(events: &[agentkit_contracts::AiRunEvent]) {
    let success = events
        .iter()
        .find(|event| event.event_type() == "run.tool.succeeded")
        .unwrap();
    let success = serde_json::to_value(success).unwrap();
    let data = &success["data"];
    let result: Value = serde_json::from_str(data["resultJson"].as_str().unwrap()).unwrap();
    assert_eq!(
        result,
        json!({"positive":null,"negative":null,"nested":[null],"rounded":9007199254740992u64,"zero":0})
    );
    let model: Value = serde_json::from_str(data["modelResultJson"].as_str().unwrap()).unwrap();
    assert_eq!(model["data"], result);
}

#[test]
fn registry_preserves_schema_identity_and_instance_literals() {
    let mut registry = ToolRegistry::new();
    let schema = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "$id":"https://schemas.example.test/root.json",
        "$defs":{"value":{"$id":"literal.json","const":{"$id":"user","$schema":"data"}}},
        "type":"object", "properties":{"payload":{"$ref":"literal.json"}},
        "required":["payload"], "unevaluatedProperties":false
    });
    registry
        .register(Arc::new(Echo {
            definition: serde_json::from_value(json!({"name":"echo","version":"1",
            "effect":"read","capability":"test","description":"Echo","inputSchema":schema}))
            .unwrap(),
            inputs: Arc::new(Mutex::new(vec![])),
        }))
        .unwrap();
    assert!(registry
        .validate_input("echo", &json!({"payload":{"$id":"user","$schema":"data"}}))
        .is_empty());
    assert!(!registry
        .validate_input("echo", &json!({"payload":{}}))
        .is_empty());
    assert!(!registry
        .validate_input(
            "echo",
            &json!({"payload":{"$id":"user","$schema":"data"},"other":1})
        )
        .is_empty());
}
