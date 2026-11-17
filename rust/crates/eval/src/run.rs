//! Drives the real run loop (`agentkit_core::run_chat`) and reduces what it
//! emitted to plain JSON, so a suite asserts on events and not on internals.
use agentkit_core::{
    ChatProvider, ChatRequest, CoreError, RunOptions, Runtime, Tool, ToolContext, ToolDefinition,
    ToolRegistry, ToolResult, run_chat,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Every event the run emitted, serialized, in emission order.
#[derive(Debug, Clone)]
pub struct Run {
    pub events: Vec<Value>,
}

impl Run {
    /// The `data` of each event of `kind`, in order.
    pub fn data(&self, kind: &str) -> Vec<&Value> {
        self.events
            .iter()
            .filter(|event| event["type"] == kind)
            .map(|event| &event["data"])
            .collect()
    }

    pub fn types(&self) -> Vec<&str> {
        self.events
            .iter()
            .filter_map(|event| event["type"].as_str())
            .collect()
    }
}

/// Run options with tool-call records on, the registry's tools offered to the
/// model (the Responses parser refuses a call to a tool it was not offered)
/// and an injected clock, so ids and timestamps do not depend on the host.
pub fn options(registry: &ToolRegistry, clock: Arc<dyn Runtime>) -> RunOptions {
    RunOptions {
        request: ChatRequest {
            model: "eval-model".into(),
            tools: registry.definitions(),
            ..Default::default()
        },
        tool_call_records: true,
        max_tool_calls_per_iteration: 16,
        runtime: clock,
        ..Default::default()
    }
}

pub async fn drive(
    options: RunOptions,
    provider: Arc<dyn ChatProvider>,
    registry: Arc<ToolRegistry>,
) -> Run {
    let handle = run_chat(options, provider, registry);
    let events = handle
        .events
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| serde_json::to_value(event).expect("event serializes"))
        .collect();
    Run { events }
}

/// A tool that answers `{"ok": true}` and keeps every argument object it was
/// executed with, in order.
pub struct RecordingTool {
    definition: ToolDefinition,
    received: Arc<Mutex<Vec<Value>>>,
}

impl RecordingTool {
    pub fn new(name: &str, effect: &str, schema: Value) -> (Arc<Self>, Arc<Mutex<Vec<Value>>>) {
        let definition = serde_json::from_value(json!({
            "name": name, "version": "1", "effect": effect, "capability": "eval.fixture",
            "description": format!("{name} for the evaluation fixtures."),
            "inputSchema": schema,
        }))
        .expect("tool definition");
        let received = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Self {
                definition,
                received: Arc::clone(&received),
            }),
            received,
        )
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError> {
        self.received.lock().expect("received lock").push(input);
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"ok": true})),
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
