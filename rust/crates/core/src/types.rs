use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{pin::Pin, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub metadata: Option<Map<String, Value>>,
}
impl ChatMessage {
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            name: None,
            tool_call_id: None,
            tool_calls: vec![],
            metadata: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub version: String,
    pub effect: String,
    pub capability: String,
    pub description: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLimits {
    pub profile: String,
    pub max_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
}
impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            profile: "small".into(),
            max_bytes: 16384,
            max_items: Some(50),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub data: Option<Value>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub sources: Vec<Value>,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub limits: ToolLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub model_data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl ToolResult {
    /// Converts host values before entering the executor's JSON-only result channel.
    pub fn serialize_data<T: Serialize>(tool_name: &str, value: &T) -> Result<Value, CoreError> {
        serde_json::to_value(value).map_err(|error| {
            CoreError::new(
                "result_unserializable",
                &format!(
                    "Tool {tool_name} produced a result that could not be serialized: {error}"
                ),
            )
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub run_id: String,
    pub chat_id: Option<String>,
    pub scope_id: Option<String>,
    pub user_id: Option<String>,
    pub bindings: Vec<Value>,
    pub limits: ToolLimits,
    pub cancellation: CancellationToken,
    /// Durable hosts supply authority; standalone calls have no task lease.
    pub dispatch_guard: Option<Arc<dyn crate::DispatchGuard>>,
    pub metadata: Map<String, Value>,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct CoreError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl CoreError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }
    pub fn responses(code: &str) -> Self {
        Self::new(code, &format!("Responses provider failed: {code}"))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;
    /// Numeric inputs follow JavaScript binary64 semantics. Overflow remains a
    /// numeric node; `agentkit_contracts::javascript_number` reads its infinity.
    async fn execute(&self, context: ToolContext, input: Value) -> Result<ToolResult, CoreError>;
}

#[derive(Clone, Default)]
pub struct ChatRequest {
    pub run_id: String,
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<u64>,
    pub continuation: Option<Value>,
    pub continuation_scope: Option<ContinuationScope>,
    pub continuation_required: bool,
    pub budget: Option<Arc<crate::ExecutionBudget>>,
    pub continuation_store: Option<Arc<dyn ContinuationStore>>,
    pub runtime: Option<Arc<dyn crate::Runtime>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationScope {
    pub chat_id: String,
    pub branch_id: String,
}

#[derive(Clone, Debug)]
pub enum ProviderEvent {
    Started,
    Delta(String),
    Completed {
        content: String,
        tool_calls: Vec<ToolCall>,
        finish_reason: String,
    },
    ToolRequested(ToolCall),
    Warning {
        code: String,
        message: String,
    },
    Usage(Value),
    Continuation(Value),
    Activity,
    Event {
        kind: String,
        data: Value,
    },
}
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, CoreError>> + Send>>;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// After cancellation, polling may report billing but must never dispatch work.
    fn supports_usage_cleanup(&self) -> bool {
        false
    }
    fn connection_identity(&self) -> Option<crate::providers::ConnectionIdentity> {
        None
    }
    fn tracks_transport_requests(&self) -> bool {
        false
    }
    fn id(&self) -> &str;
    fn stream_chat(&self, input: ChatRequest, cancellation: CancellationToken) -> ProviderStream;
    async fn list_models(&self, _cancellation: CancellationToken) -> Result<Vec<Value>, CoreError> {
        Ok(vec![])
    }
}

#[async_trait]
pub trait ContinuationStore: Send + Sync {
    async fn save(&self, state: Value) -> Result<(), CoreError>;
}

#[async_trait]
pub trait RunEventSink: Send + Sync {
    async fn append(&self, event: &agentkit_contracts::AiRunEvent) -> Result<(), CoreError>;
    async fn append_messages(
        &self,
        event: &agentkit_contracts::AiRunEvent,
        _messages: &[ChatMessage],
    ) -> Result<(), CoreError> {
        self.append(event).await
    }
}
