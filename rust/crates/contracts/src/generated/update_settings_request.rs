// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateSettingsRequest {
    #[serde(
        rename = "defaultProviderId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub default_provider_id: Optional<AiProviderKind>,
    #[serde(
        rename = "defaultModel",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub default_model: Optional<AiProviderKind>,
    #[serde(
        rename = "contextSizePreference",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub context_size_preference: Optional<AiContextSizePreference>,
    #[serde(
        rename = "writePolicyMode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub write_policy_mode: Optional<WritePolicyModeDto>,
    #[serde(
        rename = "allowRawToolData",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub allow_raw_tool_data: Optional<bool>,
    #[serde(
        rename = "maxToolIterations",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_tool_iterations: Optional<f64>,
    #[serde(
        rename = "toolCalling",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_calling: Optional<ToolCallingModeDto>,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(UpdateSettingsRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        default_provider_id: crate::optional_field(&mut object, "defaultProviderId")?,
        default_model: crate::optional_field(&mut object, "defaultModel")?,
        context_size_preference: crate::optional_field(&mut object, "contextSizePreference")?,
        write_policy_mode: crate::optional_field(&mut object, "writePolicyMode")?,
        allow_raw_tool_data: crate::optional_field(&mut object, "allowRawToolData")?,
        max_tool_iterations: crate::optional_field(&mut object, "maxToolIterations")?,
        tool_calling: crate::optional_field(&mut object, "toolCalling")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
