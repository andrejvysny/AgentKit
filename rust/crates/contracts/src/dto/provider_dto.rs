use crate::{AiProviderKind, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderDto {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "label")]
    pub label: String,
    #[serde(rename = "kind")]
    pub kind: AiProviderKind,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "defaultModel")]
    pub default_model: String,
    #[serde(rename = "enabled")]
    pub enabled: bool,
    #[serde(
        rename = "apiKeySecretRef",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub api_key_secret_ref: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ProviderDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        label: crate::required_field(&mut object, "label")?,
        kind: crate::required_field(&mut object, "kind")?,
        base_url: crate::required_field(&mut object, "baseUrl")?,
        default_model: crate::required_field(&mut object, "defaultModel")?,
        enabled: crate::required_field(&mut object, "enabled")?,
        api_key_secret_ref: crate::optional_field(&mut object, "apiKeySecretRef")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
