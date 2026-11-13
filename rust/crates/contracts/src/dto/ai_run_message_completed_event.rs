use crate::{AiCitation, AiToolCall, Optional};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunMessageCompletedEventType {
    #[serde(rename = "run.message.completed")]
    RunMessageCompleted,
}
crate::impl_json_via_serde!(AiRunMessageCompletedEventType);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunMessageCompletedEventData {
    #[serde(rename = "content")]
    pub content: String,
    #[serde(rename = "toolCallCount", serialize_with = "crate::serialize_number")]
    pub tool_call_count: f64,
    #[serde(
        rename = "toolCalls",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_calls: Optional<Vec<AiToolCall>>,
    #[serde(
        rename = "reasoningContent",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_content: Optional<String>,
    #[serde(
        rename = "finishReason",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub finish_reason: Optional<String>,
    #[serde(
        rename = "citations",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub citations: Optional<Vec<AiCitation>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunMessageCompletedEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        content: crate::required_field(&mut object, "content")?,
        tool_call_count: crate::required_field(&mut object, "toolCallCount")?,
        tool_calls: crate::optional_field(&mut object, "toolCalls")?,
        reasoning_content: crate::optional_field(&mut object, "reasoningContent")?,
        finish_reason: crate::optional_field(&mut object, "finishReason")?,
        citations: crate::optional_field(&mut object, "citations")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunMessageCompletedEvent {
    #[serde(rename = "type")]
    pub type_: AiRunMessageCompletedEventType,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "timestamp")]
    pub timestamp: String,
    #[serde(rename = "contractVersion")]
    pub contract_version: String,
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "seq", serialize_with = "crate::serialize_number")]
    pub seq: f64,
    #[serde(
        rename = "attemptId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub attempt_id: Optional<String>,
    #[serde(rename = "data")]
    pub data: AiRunMessageCompletedEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunMessageCompletedEvent, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        run_id: crate::required_field(&mut object, "runId")?,
        timestamp: crate::required_field(&mut object, "timestamp")?,
        contract_version: crate::required_field(&mut object, "contractVersion")?,
        event_id: crate::required_field(&mut object, "eventId")?,
        seq: crate::required_field(&mut object, "seq")?,
        attempt_id: crate::optional_field(&mut object, "attemptId")?,
        data: crate::required_field(&mut object, "data")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
