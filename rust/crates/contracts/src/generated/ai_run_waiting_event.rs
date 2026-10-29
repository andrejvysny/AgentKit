// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunWaitingEventType {
    #[serde(rename = "run.waiting")]
    RunWaiting,
}
crate::impl_json_via_serde!(AiRunWaitingEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunWaitingEventDataKind {
    #[serde(rename = "input")]
    Input,
    #[serde(rename = "approval")]
    Approval,
}
crate::impl_json_via_serde!(AiRunWaitingEventDataKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunWaitingEventDataStanding {
    #[serde(rename = "digest")]
    pub digest: AiProviderKind,
    #[serde(rename = "label")]
    pub label: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunWaitingEventDataStanding, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        digest: crate::required_field(&mut object, "digest")?,
        label: crate::required_field(&mut object, "label")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunWaitingEventData {
    #[serde(rename = "requestId")]
    pub request_id: AiProviderKind,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: AiProviderKind,
    #[serde(rename = "toolName")]
    pub tool_name: AiProviderKind,
    #[serde(rename = "step", serialize_with = "crate::serialize_number")]
    pub step: f64,
    #[serde(rename = "prompt")]
    pub prompt: AiProviderKind,
    #[serde(
        rename = "expiresAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub expires_at: Optional<AiProviderKind>,
    #[serde(rename = "kind", default, skip_serializing_if = "Optional::is_absent")]
    pub kind: Optional<AiRunWaitingEventDataKind>,
    #[serde(
        rename = "argsSha256",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub args_sha256: Optional<AiProviderKind>,
    #[serde(
        rename = "approvalGeneration",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub approval_generation: Optional<AiProviderKind>,
    #[serde(
        rename = "contextSha256",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub context_sha256: Optional<AiProviderKind>,
    #[serde(
        rename = "requestedMode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub requested_mode: Optional<AiProviderKind>,
    #[serde(
        rename = "standing",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub standing: Optional<AiRunWaitingEventDataStanding>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunWaitingEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        request_id: crate::required_field(&mut object, "requestId")?,
        tool_call_id: crate::required_field(&mut object, "toolCallId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        step: crate::required_field(&mut object, "step")?,
        prompt: crate::required_field(&mut object, "prompt")?,
        expires_at: crate::optional_field(&mut object, "expiresAt")?,
        kind: crate::optional_field(&mut object, "kind")?,
        args_sha256: crate::optional_field(&mut object, "argsSha256")?,
        approval_generation: crate::optional_field(&mut object, "approvalGeneration")?,
        context_sha256: crate::optional_field(&mut object, "contextSha256")?,
        requested_mode: crate::optional_field(&mut object, "requestedMode")?,
        standing: crate::optional_field(&mut object, "standing")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunWaitingEvent {
    #[serde(rename = "type")]
    pub type_: AiRunWaitingEventType,
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
    pub data: AiRunWaitingEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunWaitingEvent, |value: Value| {
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
