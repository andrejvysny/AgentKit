use crate::{AiSourceRef, Optional};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunToolSucceededEventType {
    #[serde(rename = "run.tool.succeeded")]
    RunToolSucceeded,
}
crate::impl_json_via_serde!(AiRunToolSucceededEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunToolSucceededEventDataStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "partial")]
    Partial,
}
crate::impl_json_via_serde!(AiRunToolSucceededEventDataStatus);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunToolSucceededEventData {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "resultJson")]
    pub result_json: String,
    #[serde(rename = "sources")]
    pub sources: Vec<AiSourceRef>,
    #[serde(rename = "truncated")]
    pub truncated: bool,
    #[serde(rename = "warnings")]
    pub warnings: Vec<String>,
    #[serde(
        rename = "status",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub status: Optional<AiRunToolSucceededEventDataStatus>,
    #[serde(
        rename = "summary",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub summary: Optional<String>,
    #[serde(
        rename = "modelResultJson",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub model_result_json: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunToolSucceededEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        tool_call_id: crate::required_field(&mut object, "toolCallId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        result_json: crate::required_field(&mut object, "resultJson")?,
        sources: crate::required_field(&mut object, "sources")?,
        truncated: crate::required_field(&mut object, "truncated")?,
        warnings: crate::required_field(&mut object, "warnings")?,
        status: crate::optional_field(&mut object, "status")?,
        summary: crate::optional_field(&mut object, "summary")?,
        model_result_json: crate::optional_field(&mut object, "modelResultJson")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunToolSucceededEvent {
    #[serde(rename = "type")]
    pub type_: AiRunToolSucceededEventType,
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
    pub data: AiRunToolSucceededEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunToolSucceededEvent, |value: Value| {
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
