// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunWarningEventType {
    #[serde(rename = "run.warning")]
    RunWarning,
}
crate::impl_json_via_serde!(AiRunWarningEventType);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunWarningEventData {
    #[serde(rename = "code")]
    pub code: AiProviderKind,
    #[serde(rename = "message")]
    pub message: AiProviderKind,
    #[serde(
        rename = "pass",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub pass: Optional<f64>,
    #[serde(
        rename = "reason",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reason: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunWarningEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        code: crate::required_field(&mut object, "code")?,
        message: crate::required_field(&mut object, "message")?,
        pass: crate::optional_field(&mut object, "pass")?,
        reason: crate::optional_field(&mut object, "reason")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunWarningEvent {
    #[serde(rename = "type")]
    pub type_: AiRunWarningEventType,
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
    pub data: AiRunWarningEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunWarningEvent, |value: Value| {
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
