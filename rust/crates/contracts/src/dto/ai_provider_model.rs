use crate::{Nullable, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiProviderModel {
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    #[serde(rename = "displayName")]
    pub display_name: Nullable<String>,
    #[serde(
        rename = "contextWindowTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub context_window_tokens: Optional<f64>,
    #[serde(
        rename = "supportsToolCalling",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub supports_tool_calling: Optional<bool>,
    #[serde(rename = "fetchedAt")]
    pub fetched_at: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiProviderModel, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        provider_id: crate::required_field(&mut object, "providerId")?,
        model_id: crate::required_field(&mut object, "modelId")?,
        display_name: crate::required_field(&mut object, "displayName")?,
        context_window_tokens: crate::optional_field(&mut object, "contextWindowTokens")?,
        supports_tool_calling: crate::optional_field(&mut object, "supportsToolCalling")?,
        fetched_at: crate::required_field(&mut object, "fetchedAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
