//! Scripted ports used by the context capture example and replay evaluations.

use crate::prefix_harness::EventMark;
use agentkit_core::{
    AuthKind, ConnectionIdentity, CoreError, Tool, ToolContext, ToolDefinition, ToolResult,
};
use agentkit_engine::{EventRecord, TaskRepository};
use agentkit_governance::{
    ContributedTool, GovernanceResult, ToolContributionContext, ToolContributor,
};
use agentkit_providers::TrustedTransport;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

pub struct RecordingTransport<R: TaskRepository> {
    protocol: &'static str,
    sent: Mutex<Vec<(Value, EventMark)>>,
    store: R,
    current: Mutex<(usize, String)>,
    frames: Vec<String>,
}

impl<R: TaskRepository> RecordingTransport<R> {
    pub fn new(protocol: &'static str, frames: Vec<String>, store: R) -> Self {
        Self {
            protocol,
            frames,
            store,
            sent: Mutex::new(vec![]),
            current: Mutex::new((0, String::new())),
        }
    }

    pub fn set_current(&self, turn: usize, task: &str) {
        *self.current.lock().expect("fixture current lock") = (turn, task.into());
    }

    pub fn requests(&self) -> Vec<(Value, EventMark)> {
        self.sent.lock().expect("fixture request lock").clone()
    }
}

#[async_trait]
impl<R: TaskRepository> TrustedTransport for RecordingTransport<R> {
    fn identity(&self) -> ConnectionIdentity {
        ConnectionIdentity {
            provider_id: "test".into(),
            protocol: self.protocol.into(),
            connection_id: "c".into(),
            generation: 1,
        }
    }

    fn auth_kind(&self) -> AuthKind {
        AuthKind::ApiKey
    }

    async fn request(
        &self,
        _: &ConnectionIdentity,
        _: &str,
        body: Option<Value>,
        _: CancellationToken,
    ) -> Result<reqwest::Response, CoreError> {
        let (turn, task) = self.current.lock().expect("fixture current lock").clone();
        let events = recorded_events(&self.store, &task)
            .await
            .map_err(|_| CoreError::new("fixture_events", "fixture event read failed"))?;
        let mark = EventMark {
            turn,
            seq: events.last().map_or(-1.0, |event| event.seq),
        };
        let mut sent = self.sent.lock().expect("fixture request lock");
        let body = body.ok_or_else(|| CoreError::new("fixture_body", "missing fixture request"))?;
        sent.push((body, mark));
        let frame = self
            .frames
            .get(sent.len() - 1)
            .cloned()
            .unwrap_or_else(|| default_frame(self.protocol, sent.len() % 2 == 1));
        Ok(http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(frame)
            .expect("fixture HTTP response")
            .into())
    }
}

fn default_frame(protocol: &str, tool: bool) -> String {
    let frame = if protocol == "chat-completions" {
        if tool {
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read_file","arguments":"{ \"path\": \"a.txt\" }"}}]},"finish_reason":"tool_calls"}]})
        } else {
            json!({"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]})
        }
    } else if tool {
        json!({"type":"response.completed","response":{"status":"completed","output":[{"id":"fc1","type":"function_call","call_id":"call-1","name":"read_file","namespace":"agentkit","arguments":"{ \"path\": \"a.txt\" }"}]}})
    } else {
        json!({"type":"response.completed","response":{"status":"completed","output":[{"id":"m1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]}]}})
    };
    format!("data: {frame}\n\ndata: [DONE]\n\n")
}

pub struct ScriptedReadTool {
    definition: ToolDefinition,
    count: AtomicUsize,
    results: Vec<String>,
}

impl ScriptedReadTool {
    pub fn new(name: &str, description: &str, results: Vec<String>) -> Self {
        Self {
            definition: ToolDefinition {
                name: name.into(),
                version: "1".into(),
                effect: "read".into(),
                capability: "fs.read".into(),
                description: description.into(),
                input_schema: json!({"type":"object"}),
                output_schema: None,
                timeout_ms: None,
            },
            count: AtomicUsize::new(0),
            results,
        }
    }
}

#[async_trait]
impl Tool for ScriptedReadTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(&self, context: ToolContext, _: Value) -> Result<ToolResult, CoreError> {
        let text = self
            .results
            .get(self.count.fetch_add(1, Ordering::SeqCst))
            .cloned()
            .unwrap_or_default();
        Ok(ToolResult {
            ok: true,
            data: Some(json!({"text": text})),
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

pub struct SwitchableContributor {
    enabled: Arc<AtomicBool>,
    tool: Arc<dyn Tool>,
}

impl SwitchableContributor {
    pub fn new(enabled: Arc<AtomicBool>, tool: Arc<dyn Tool>) -> Self {
        Self { enabled, tool }
    }
}

#[async_trait]
impl ToolContributor for SwitchableContributor {
    fn namespace(&self) -> &str {
        "fixture"
    }

    async fn contribute(
        &self,
        _: &ToolContributionContext,
    ) -> GovernanceResult<Vec<ContributedTool>> {
        Ok(if self.enabled.load(Ordering::SeqCst) {
            vec![ContributedTool::Read(self.tool.clone())]
        } else {
            vec![]
        })
    }
}

/// Read the complete durable log; dispatch marks must not stop at a page boundary.
pub async fn recorded_events<R: TaskRepository>(
    store: &R,
    task: &str,
) -> Result<Vec<EventRecord>, R::Error> {
    let mut result = Vec::new();
    let mut cursor = None;
    loop {
        let page = store.events_after_number(task, cursor, 1000).await?;
        if page.is_empty() {
            return Ok(result);
        }
        for event in &page {
            assert!(
                event.seq.is_finite() && cursor.is_none_or(|value| event.seq > value),
                "event cursor must advance"
            );
            cursor = Some(event.seq);
        }
        result.extend(page);
    }
}
