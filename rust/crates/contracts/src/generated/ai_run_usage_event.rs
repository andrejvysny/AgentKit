// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunUsageEventType {
    #[serde(rename = "run.usage")]
    RunUsage,
}
crate::impl_json_via_serde!(AiRunUsageEventType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunUsageEventDataSource {
    #[serde(rename = "stream")]
    Stream,
    #[serde(rename = "response")]
    Response,
}
crate::impl_json_via_serde!(AiRunUsageEventDataSource);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunUsageEventData {
    #[serde(rename = "callId")]
    pub call_id: AiProviderKind,
    #[serde(rename = "attempt", serialize_with = "crate::serialize_number")]
    pub attempt: f64,
    #[serde(rename = "step", serialize_with = "crate::serialize_number")]
    pub step: f64,
    #[serde(rename = "model")]
    pub model: AiProviderKind,
    #[serde(
        rename = "providerId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub provider_id: Optional<AiProviderKind>,
    #[serde(
        rename = "promptTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub prompt_tokens: Optional<f64>,
    #[serde(
        rename = "completionTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub completion_tokens: Optional<f64>,
    #[serde(
        rename = "totalTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub total_tokens: Optional<f64>,
    #[serde(
        rename = "cachedPromptTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub cached_prompt_tokens: Optional<f64>,
    #[serde(
        rename = "reasoningTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub reasoning_tokens: Optional<f64>,
    #[serde(rename = "source")]
    pub source: AiRunUsageEventDataSource,
    #[serde(rename = "finalForCall")]
    pub final_for_call: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunUsageEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        call_id: crate::required_field(&mut object, "callId")?,
        attempt: crate::required_field(&mut object, "attempt")?,
        step: crate::required_field(&mut object, "step")?,
        model: crate::required_field(&mut object, "model")?,
        provider_id: crate::optional_field(&mut object, "providerId")?,
        prompt_tokens: crate::optional_field(&mut object, "promptTokens")?,
        completion_tokens: crate::optional_field(&mut object, "completionTokens")?,
        total_tokens: crate::optional_field(&mut object, "totalTokens")?,
        cached_prompt_tokens: crate::optional_field(&mut object, "cachedPromptTokens")?,
        reasoning_tokens: crate::optional_field(&mut object, "reasoningTokens")?,
        source: crate::required_field(&mut object, "source")?,
        final_for_call: crate::required_field(&mut object, "finalForCall")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunUsageEvent {
    #[serde(rename = "type")]
    pub type_: AiRunUsageEventType,
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
    pub data: AiRunUsageEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunUsageEvent, |value: Value| {
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
