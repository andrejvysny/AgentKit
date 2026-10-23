// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunVerificationEventType {
    #[serde(rename = "run.verification")]
    RunVerification,
}
crate::impl_json_via_serde!(AiRunVerificationEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunVerificationEventDataStatus {
    #[serde(rename = "pass")]
    Pass,
    #[serde(rename = "partial")]
    Partial,
    #[serde(rename = "unavailable")]
    Unavailable,
}
crate::impl_json_via_serde!(AiRunVerificationEventDataStatus);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunVerificationEventData {
    #[serde(rename = "pass", serialize_with = "crate::serialize_number")]
    pub pass: f64,
    #[serde(rename = "status")]
    pub status: AiRunVerificationEventDataStatus,
    #[serde(rename = "deficiencies")]
    pub deficiencies: Vec<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunVerificationEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        pass: crate::required_field(&mut object, "pass")?,
        status: crate::required_field(&mut object, "status")?,
        deficiencies: crate::required_field(&mut object, "deficiencies")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunVerificationEvent {
    #[serde(rename = "type")]
    pub type_: AiRunVerificationEventType,
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
    pub data: AiRunVerificationEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunVerificationEvent, |value: Value| {
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
