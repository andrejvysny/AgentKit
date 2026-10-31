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

    /// True for the synthesized `tool_result_missing` failure the host's
    /// history reconciliation injects when a persisted declaration has no
    /// result. Once the run itself answers the call (approved resume,
    /// replay), this fabricated failure is superseded evidence and must
    /// not sit beside the real result.
    pub fn is_synthetic_missing(&self) -> bool {
        let code = |value: &Value| {
            value
                .get("data")
                .and_then(|data| data.get("errorCode"))
                .and_then(Value::as_str)
                == Some(TOOL_RESULT_MISSING_CODE)
        };
        self.role == "tool"
            && match &self.content {
                Value::String(text) => serde_json::from_str::<Value>(text)
                    .ok()
                    .is_some_and(|value| code(&value)),
                value => code(value),
            }
    }
}

/// Marker code carried by the synthesized "missing tool result" record
/// (see [`ChatMessage::is_synthetic_missing`]).
pub const TOOL_RESULT_MISSING_CODE: &str = "tool_result_missing";

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
    /// Stable identity of the model's logical tool call. Hosts bind durable
    /// execution receipts to it; it never changes across attempts.
    pub tool_call_id: Option<String>,
    /// Durable execution attempt this call runs under, from the task lease.
    pub attempt_id: Option<String>,
    pub chat_id: Option<String>,
    pub scope_id: Option<String>,
    pub user_id: Option<String>,
    pub bindings: Vec<Value>,
    pub limits: ToolLimits,
    pub cancellation: CancellationToken,
    /// Durable hosts supply authority; standalone calls have no task lease.
    pub dispatch_guard: Option<Arc<dyn crate::DispatchGuard>>,
    pub metadata: Map<String, Value>,
    /// Filled by a tool that must wait for a human instead of answering;
    /// `None` in hosts that cannot park (the tool must then deny).
    pub suspension: Option<Arc<SuspendSlot>>,
}

impl ToolContext {
    /// The deterministic request id for this call (`hreq:<run>:<call>`):
    /// the same logical request across attempts and retries.
    pub fn waiting_request_id(&self) -> String {
        format!(
            "hreq:{}:{}",
            self.run_id,
            self.tool_call_id.as_deref().unwrap_or("")
        )
    }
    /// Ask a human and park the run. False when this host cannot park: the
    /// tool must then produce its own refusal, never a silent success.
    pub fn request_human(&self, request: HumanRequestSpec) -> bool {
        match &self.suspension {
            Some(slot) => {
                slot.set(request);
                true
            }
            None => false,
        }
    }
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
    /// Effect-relevant context this tool executes inside (workspace root,
    /// sandbox/network policy, runner selection, …). Durable execution
    /// binds it into the execution-context fingerprint so a recorded
    /// result can only be replayed for the SAME execution environment.
    /// `None` contributes an empty object; tools with no ambient effect
    /// context keep the default.
    fn effect_context(&self) -> Option<Value> {
        None
    }
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
    RequestStarted,
    RetryBackoff(Arc<tokio::sync::Notify>),
    Event {
        kind: String,
        data: Value,
    },
}
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, CoreError>> + Send>>;

#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Whether the host may add an output limit when the caller omitted one.
    fn allows_implicit_output_limit(&self) -> bool {
        true
    }
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

#[async_trait]
pub trait RunHooks: Send + Sync {
    fn indexes(&self, name: &str) -> Vec<usize>;
    async fn call(
        &self,
        name: &str,
        index: usize,
        context: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value, CoreError>;
}

/// A tool's typed request to park the run until a human answers. The host
/// persists it as the durable human-request record of the parked turn
/// (ADR 0016 §2); nothing about this decision is inferred from result text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HumanRequestSpec {
    /// Deterministic request id (`hreq:<taskId>:<toolCallId>`); the store's
    /// idempotency key across attempts and retries.
    pub request_id: String,
    /// The request record's content: `kind` (`input` | `approval`),
    /// `toolName`, `argsSha256`, `prompt`, optional `standing` for
    /// exact-call remembering.
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub content: Value,
    /// Digest of `content`; how a replayed answer is told from a conflict.
    pub content_hash: String,
    /// ISO-8601 expiry; `None` never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// The one-shot channel a tool uses to hand the run its human request.
/// Filling it suspends the run at the tool boundary instead of recording a
/// result for the call.
#[derive(Debug, Default)]
pub struct SuspendSlot {
    request: std::sync::Mutex<Option<HumanRequestSpec>>,
}

impl SuspendSlot {
    /// Called by the tool; the first request wins, later ones are ignored
    /// because the run stops at the first.
    pub fn set(&self, request: HumanRequestSpec) {
        let mut slot = self
            .request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(request);
        }
    }
    pub(crate) fn take(&self) -> Option<HumanRequestSpec> {
        self.request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// The host's park authority: called once the run has durably recorded its
/// `run.waiting` event. The host commits the parked turn (status, attempt,
/// lease, request record) here; returning `Err` fails the run honestly.
#[async_trait]
pub trait SuspendSink: Send + Sync {
    async fn park(&self, request: &HumanRequestSpec, tool_call_id: &str) -> Result<(), CoreError>;
}

/// One steer the host offers the run at a tool boundary (ADR 0016 §3):
/// the durable input's id and the user message it places.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteeringItem {
    pub input_id: String,
    pub message: ChatMessage,
}

/// The host's pending steers for this run. The loop polls ONLY at a tool
/// boundary; a source that throws or hangs costs the run nothing — the steers
/// stay pending with the host and become follow-ups at settlement.
#[async_trait]
pub trait SteeringSource: Send + Sync {
    async fn pending(&self) -> Vec<SteeringItem>;
    async fn pending_context(
        &self,
        _run_id: &str,
        _step: u32,
    ) -> Result<Vec<SteeringItem>, CoreError> {
        Ok(self.pending().await)
    }
}
