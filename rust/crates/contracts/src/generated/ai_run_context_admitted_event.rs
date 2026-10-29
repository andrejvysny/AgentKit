// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunContextAdmittedEventType {
    #[serde(rename = "run.context.admitted")]
    RunContextAdmitted,
}
crate::impl_json_via_serde!(AiRunContextAdmittedEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunContextAdmittedEventDataWindowSource {
    #[serde(rename = "descriptor")]
    Descriptor,
    #[serde(rename = "model")]
    Model,
    #[serde(rename = "capabilities")]
    Capabilities,
    #[serde(rename = "fallback")]
    Fallback,
}
crate::impl_json_via_serde!(AiRunContextAdmittedEventDataWindowSource);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunContextAdmittedEventData {
    #[serde(rename = "requestId")]
    pub request_id: AiProviderKind,
    #[serde(rename = "windowTokens", serialize_with = "crate::serialize_number")]
    pub window_tokens: f64,
    #[serde(rename = "windowSource")]
    pub window_source: AiRunContextAdmittedEventDataWindowSource,
    #[serde(rename = "reserveTokens", serialize_with = "crate::serialize_number")]
    pub reserve_tokens: f64,
    #[serde(rename = "toolTokens", serialize_with = "crate::serialize_number")]
    pub tool_tokens: f64,
    #[serde(rename = "extraTokens", serialize_with = "crate::serialize_number")]
    pub extra_tokens: f64,
    #[serde(
        rename = "estimatedInputTokens",
        serialize_with = "crate::serialize_number"
    )]
    pub estimated_input_tokens: f64,
    #[serde(rename = "budgetTokens", serialize_with = "crate::serialize_number")]
    pub budget_tokens: f64,
    #[serde(
        rename = "maxOutputTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_output_tokens: Optional<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunContextAdmittedEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        request_id: crate::required_field(&mut object, "requestId")?,
        window_tokens: crate::required_field(&mut object, "windowTokens")?,
        window_source: crate::required_field(&mut object, "windowSource")?,
        reserve_tokens: crate::required_field(&mut object, "reserveTokens")?,
        tool_tokens: crate::required_field(&mut object, "toolTokens")?,
        extra_tokens: crate::required_field(&mut object, "extraTokens")?,
        estimated_input_tokens: crate::required_field(&mut object, "estimatedInputTokens")?,
        budget_tokens: crate::required_field(&mut object, "budgetTokens")?,
        max_output_tokens: crate::optional_field(&mut object, "maxOutputTokens")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunContextAdmittedEvent {
    #[serde(rename = "type")]
    pub type_: AiRunContextAdmittedEventType,
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
    pub data: AiRunContextAdmittedEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunContextAdmittedEvent, |value: Value| {
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
