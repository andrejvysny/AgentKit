// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunStartedEventType {
    #[serde(rename = "run.started")]
    RunStarted,
}
crate::impl_json_via_serde!(AiRunStartedEventType);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunStartedEventData {
    #[serde(rename = "model")]
    pub model: AiProviderKind,
    #[serde(rename = "toolCount", serialize_with = "crate::serialize_number")]
    pub tool_count: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunStartedEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        model: crate::required_field(&mut object, "model")?,
        tool_count: crate::required_field(&mut object, "toolCount")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunStartedEvent {
    #[serde(rename = "type")]
    pub type_: AiRunStartedEventType,
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
    pub data: AiRunStartedEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunStartedEvent, |value: Value| {
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
