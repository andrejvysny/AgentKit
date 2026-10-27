// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunContextCompactedEventType {
    #[serde(rename = "run.context.compacted")]
    RunContextCompacted,
}
crate::impl_json_via_serde!(AiRunContextCompactedEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunContextCompactedEventDataReason {
    #[serde(rename = "threshold")]
    Threshold,
    #[serde(rename = "manual")]
    Manual,
}
crate::impl_json_via_serde!(AiRunContextCompactedEventDataReason);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunContextCompactedEventData {
    #[serde(rename = "summaryId")]
    pub summary_id: AiProviderKind,
    #[serde(rename = "tokensBefore", serialize_with = "crate::serialize_number")]
    pub tokens_before: f64,
    #[serde(rename = "tokensAfter", serialize_with = "crate::serialize_number")]
    pub tokens_after: f64,
    #[serde(rename = "coveredMessages", serialize_with = "crate::serialize_number")]
    pub covered_messages: f64,
    #[serde(rename = "reason")]
    pub reason: AiRunContextCompactedEventDataReason,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunContextCompactedEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        summary_id: crate::required_field(&mut object, "summaryId")?,
        tokens_before: crate::required_field(&mut object, "tokensBefore")?,
        tokens_after: crate::required_field(&mut object, "tokensAfter")?,
        covered_messages: crate::required_field(&mut object, "coveredMessages")?,
        reason: crate::required_field(&mut object, "reason")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunContextCompactedEvent {
    #[serde(rename = "type")]
    pub type_: AiRunContextCompactedEventType,
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
    pub data: AiRunContextCompactedEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunContextCompactedEvent, |value: Value| {
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
