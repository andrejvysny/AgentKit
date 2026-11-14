use crate::Optional;
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
    pub call_id: String,
    #[serde(rename = "attempt", serialize_with = "crate::serialize_number")]
    pub attempt: f64,
    #[serde(rename = "step", serialize_with = "crate::serialize_number")]
    pub step: f64,
    #[serde(rename = "model")]
    pub model: String,
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
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub input_uncached: Optional<u64>,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub cache_read: Optional<u64>,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub cache_write_short: Optional<u64>,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub cache_write_long: Optional<u64>,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub output: Optional<u64>,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub reasoning: Optional<u64>,
    #[serde(default, skip_serializing_if = "Optional::is_absent")]
    pub provider_raw: Optional<Value>,
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
        prompt_tokens: crate::optional_field(&mut object, "promptTokens")?,
        completion_tokens: crate::optional_field(&mut object, "completionTokens")?,
        total_tokens: crate::optional_field(&mut object, "totalTokens")?,
        cached_prompt_tokens: crate::optional_field(&mut object, "cachedPromptTokens")?,
        reasoning_tokens: crate::optional_field(&mut object, "reasoningTokens")?,
        input_uncached: optional_safe_counter(&mut object, "input_uncached")?,
        cache_read: optional_safe_counter(&mut object, "cache_read")?,
        cache_write_short: optional_safe_counter(&mut object, "cache_write_short")?,
        cache_write_long: optional_safe_counter(&mut object, "cache_write_long")?,
        output: optional_safe_counter(&mut object, "output")?,
        reasoning: optional_safe_counter(&mut object, "reasoning")?,
        provider_raw: crate::optional_field(&mut object, "provider_raw")?,
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

fn optional_safe_counter(
    object: &mut serde_json::Map<String, Value>,
    name: &str,
) -> Result<Optional<u64>, serde_json::Error> {
    let Some(value) = object.remove(name) else {
        return Ok(Optional::Absent);
    };
    let number = value
        .as_f64()
        .filter(|number| {
            number.is_finite()
                && number.fract() == 0.0
                && (0.0..=9_007_199_254_740_991.0).contains(number)
        })
        .ok_or_else(|| crate::json_error("usage counter must be a non-negative safe integer"))?;
    Ok(Optional::Present(number as u64))
}
