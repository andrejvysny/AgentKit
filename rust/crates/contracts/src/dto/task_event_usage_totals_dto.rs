use crate::{Nullable, Optional};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskEventUsageTotalsDto {
    #[serde(
        rename = "promptTokens",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub prompt_tokens: Optional<Nullable<f64>>,
    #[serde(
        rename = "completionTokens",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub completion_tokens: Optional<Nullable<f64>>,
    #[serde(
        rename = "totalTokens",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub total_tokens: Optional<Nullable<f64>>,
    #[serde(
        rename = "cachedPromptTokens",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub cached_prompt_tokens: Optional<Nullable<f64>>,
    #[serde(
        rename = "reasoningTokens",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_tokens: Optional<Nullable<f64>>,
    #[serde(rename = "usageEvents", serialize_with = "crate::serialize_number")]
    pub usage_events: f64,
}
crate::impl_json_deserialize!(TaskEventUsageTotalsDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        prompt_tokens: crate::optional_field(&mut object, "promptTokens")?,
        completion_tokens: crate::optional_field(&mut object, "completionTokens")?,
        total_tokens: crate::optional_field(&mut object, "totalTokens")?,
        cached_prompt_tokens: crate::optional_field(&mut object, "cachedPromptTokens")?,
        reasoning_tokens: crate::optional_field(&mut object, "reasoningTokens")?,
        usage_events: crate::required_field(&mut object, "usageEvents")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
