// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiProviderConfig {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "label")]
    pub label: AiProviderKind,
    #[serde(rename = "kind")]
    pub kind: AiProviderKind,
    #[serde(rename = "baseUrl")]
    pub base_url: AiProviderKind,
    #[serde(
        rename = "apiKey",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub api_key: Optional<AiProviderKind>,
    #[serde(rename = "defaultModel")]
    pub default_model: AiProviderKind,
    #[serde(rename = "enabled")]
    pub enabled: bool,
    #[serde(
        rename = "extraHeaders",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub extra_headers: Optional<BTreeMap<String, AiProviderKind>>,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiProviderConfig, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        label: crate::required_field(&mut object, "label")?,
        kind: crate::required_field(&mut object, "kind")?,
        base_url: crate::required_field(&mut object, "baseUrl")?,
        api_key: crate::optional_field(&mut object, "apiKey")?,
        default_model: crate::required_field(&mut object, "defaultModel")?,
        enabled: crate::required_field(&mut object, "enabled")?,
        extra_headers: crate::optional_field(&mut object, "extraHeaders")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
