// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunToolFailedEventType {
    #[serde(rename = "run.tool.failed")]
    RunToolFailed,
}
crate::impl_json_via_serde!(AiRunToolFailedEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunToolFailedEventDataStatus {
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "partial")]
    Partial,
}
crate::impl_json_via_serde!(AiRunToolFailedEventDataStatus);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunToolFailedEventData {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: AiProviderKind,
    #[serde(rename = "toolName")]
    pub tool_name: AiProviderKind,
    #[serde(rename = "errorMessage")]
    pub error_message: AiProviderKind,
    #[serde(
        rename = "errorCode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub error_code: Optional<AiProviderKind>,
    #[serde(
        rename = "modelResultJson",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub model_result_json: Optional<AiProviderKind>,
    #[serde(
        rename = "status",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub status: Optional<AiRunToolFailedEventDataStatus>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunToolFailedEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        tool_call_id: crate::required_field(&mut object, "toolCallId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        error_message: crate::required_field(&mut object, "errorMessage")?,
        error_code: crate::optional_field(&mut object, "errorCode")?,
        model_result_json: crate::optional_field(&mut object, "modelResultJson")?,
        status: crate::optional_field(&mut object, "status")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunToolFailedEvent {
    #[serde(rename = "type")]
    pub type_: AiRunToolFailedEventType,
    #[serde(rename = "runId")]
    pub run_id: AiProviderKind,
    #[serde(rename = "timestamp")]
    pub timestamp: AiProviderKind,
    #[serde(rename = "contractVersion")]
    pub contract_version: AiProviderKind,
    #[serde(rename = "eventId")]
    pub event_id: AiProviderKind,
    #[serde(rename = "seq", serialize_with = "crate::serialize_number")]
    pub seq: f64,
    #[serde(
        rename = "attemptId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub attempt_id: Optional<AiProviderKind>,
    #[serde(rename = "data")]
    pub data: AiRunToolFailedEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunToolFailedEvent, |value: Value| {
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
