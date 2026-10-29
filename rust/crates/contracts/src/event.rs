use crate::{validate_schema, AiRunEvent, Optional, CONTRACT_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Transport metadata shared by all run event variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventMetadata {
    pub run_id: String,
    pub timestamp: String,
    pub contract_version: String,
    pub event_id: String,
    #[serde(serialize_with = "crate::serialize_number")]
    pub seq: f64,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub attempt_id: Optional<String>,
}

impl EventMetadata {
    pub fn new(run_id: &str, timestamp: &str, event_id: &str, seq: f64) -> Self {
        Self {
            run_id: run_id.into(),
            timestamp: timestamp.into(),
            contract_version: CONTRACT_VERSION.into(),
            event_id: event_id.into(),
            seq,
            attempt_id: Optional::Absent,
        }
    }
}

impl AiRunEvent {
    /// Validate first so JSON Schema constraints also apply to generated DTOs.
    pub fn from_parts(
        event_type: &str,
        metadata: &EventMetadata,
        data: Value,
    ) -> Result<Self, crate::ValidationError> {
        let mut value = serde_json::to_value(metadata)?;
        value["type"] = Value::String(event_type.into());
        value["data"] = data;
        validate_schema("AiRunEventSchema", &value)?;
        Ok(serde_json::from_value(value)?)
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Started(_) => "run.started",
            Self::MessageDelta(_) => "run.message.delta",
            Self::MessageReasoningDelta(_) => "run.message.reasoning-delta",
            Self::MessageCompleted(_) => "run.message.completed",
            Self::ToolRequested(_) => "run.tool.requested",
            Self::ToolRunning(_) => "run.tool.running",
            Self::ToolSucceeded(_) => "run.tool.succeeded",
            Self::ToolFailed(_) => "run.tool.failed",
            Self::Warning(_) => "run.warning",
            Self::Usage(_) => "run.usage",
            Self::ContextCompacted(_) => "run.context.compacted",
            Self::ContextAdmitted(_) => "run.context.admitted",
            Self::Verification(_) => "run.verification",
            Self::InputApplied(_) => "run.input.applied",
            Self::Waiting(_) => "run.waiting",
            Self::Completed(_) => "run.completed",
            Self::Failed(_) => "run.failed",
            Self::Cancelled(_) => "run.cancelled",
        }
    }
}
