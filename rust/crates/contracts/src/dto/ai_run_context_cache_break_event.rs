use crate::Optional;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunContextCacheBreakEventType {
    #[serde(rename = "context.cache_break")]
    ContextCacheBreak,
}
crate::impl_json_via_serde!(AiRunContextCacheBreakEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunContextCacheBreakEventDataReason {
    #[serde(rename = "params")]
    Params,
    #[serde(rename = "mask")]
    Mask,
    #[serde(rename = "compact")]
    Compact,
    #[serde(rename = "refreeze")]
    Refreeze,
}
crate::impl_json_via_serde!(AiRunContextCacheBreakEventDataReason);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunContextCacheBreakEventData {
    #[serde(rename = "reason")]
    pub reason: AiRunContextCacheBreakEventDataReason,
    #[serde(
        rename = "tokens_before",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub tokens_before: Optional<f64>,
    #[serde(
        rename = "tokens_after",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub tokens_after: Optional<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunContextCacheBreakEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        reason: crate::required_field(&mut object, "reason")?,
        tokens_before: crate::optional_field(&mut object, "tokens_before")?,
        tokens_after: crate::optional_field(&mut object, "tokens_after")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunContextCacheBreakEvent {
    #[serde(rename = "type")]
    pub type_: AiRunContextCacheBreakEventType,
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
    pub data: AiRunContextCacheBreakEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunContextCacheBreakEvent, |value: Value| {
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
