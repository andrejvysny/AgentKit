use crate::{Nullable, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskEventUsage {
    #[serde(
        rename = "callId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub call_id: Optional<String>,
    #[serde(rename = "model", default, skip_serializing_if = "Optional::is_absent")]
    pub model: Optional<String>,
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
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(TaskEventUsage, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        call_id: crate::optional_field(&mut object, "callId")?,
        model: crate::optional_field(&mut object, "model")?,
        prompt_tokens: crate::optional_field(&mut object, "promptTokens")?,
        completion_tokens: crate::optional_field(&mut object, "completionTokens")?,
        total_tokens: crate::optional_field(&mut object, "totalTokens")?,
        cached_prompt_tokens: crate::optional_field(&mut object, "cachedPromptTokens")?,
        reasoning_tokens: crate::optional_field(&mut object, "reasoningTokens")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
