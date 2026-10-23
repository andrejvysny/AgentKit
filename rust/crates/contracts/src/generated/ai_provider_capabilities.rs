// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiProviderCapabilities {
    #[serde(rename = "streaming")]
    pub streaming: bool,
    #[serde(rename = "toolCalling")]
    pub tool_calling: bool,
    #[serde(rename = "modelList")]
    pub model_list: bool,
    #[serde(
        rename = "vision",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub vision: Optional<bool>,
    #[serde(rename = "audio", default, skip_serializing_if = "Optional::is_absent")]
    pub audio: Optional<bool>,
    #[serde(
        rename = "jsonMode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub json_mode: Optional<bool>,
    #[serde(
        rename = "maxContextTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_context_tokens: Optional<f64>,
    #[serde(
        rename = "checkedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub checked_at: Optional<AiProviderKind>,
    #[serde(
        rename = "warning",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub warning: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiProviderCapabilities, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        streaming: crate::required_field(&mut object, "streaming")?,
        tool_calling: crate::required_field(&mut object, "toolCalling")?,
        model_list: crate::required_field(&mut object, "modelList")?,
        vision: crate::optional_field(&mut object, "vision")?,
        audio: crate::optional_field(&mut object, "audio")?,
        json_mode: crate::optional_field(&mut object, "jsonMode")?,
        max_context_tokens: crate::optional_field(&mut object, "maxContextTokens")?,
        checked_at: crate::optional_field(&mut object, "checkedAt")?,
        warning: crate::optional_field(&mut object, "warning")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
