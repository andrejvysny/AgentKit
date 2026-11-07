// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunProgressEventType {
    #[serde(rename = "run.progress")]
    RunProgress,
}
crate::impl_json_via_serde!(AiRunProgressEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunProgressEventDataPhase {
    #[serde(rename = "reasoning")]
    Reasoning,
    #[serde(rename = "generating")]
    Generating,
    #[serde(rename = "tool_args")]
    ToolArgs,
}
crate::impl_json_via_serde!(AiRunProgressEventDataPhase);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunProgressEventData {
    #[serde(rename = "phase")]
    pub phase: AiRunProgressEventDataPhase,
    #[serde(rename = "step", serialize_with = "crate::serialize_number")]
    pub step: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunProgressEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        phase: crate::required_field(&mut object, "phase")?,
        step: crate::required_field(&mut object, "step")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunProgressEvent {
    #[serde(rename = "type")]
    pub type_: AiRunProgressEventType,
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
    pub data: AiRunProgressEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunProgressEvent, |value: Value| {
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
