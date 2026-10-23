// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateProviderRequest {
    #[serde(rename = "label", default, skip_serializing_if = "Optional::is_absent")]
    pub label: Optional<AiProviderKind>,
    #[serde(rename = "kind", default, skip_serializing_if = "Optional::is_absent")]
    pub kind: Optional<AiProviderKind>,
    #[serde(
        rename = "baseUrl",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub base_url: Optional<AiProviderKind>,
    #[serde(
        rename = "defaultModel",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub default_model: Optional<AiProviderKind>,
    #[serde(
        rename = "enabled",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub enabled: Optional<bool>,
    #[serde(
        rename = "apiKey",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub api_key: Optional<AiProviderKind>,
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
crate::impl_json_deserialize!(UpdateProviderRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        label: crate::optional_field(&mut object, "label")?,
        kind: crate::optional_field(&mut object, "kind")?,
        base_url: crate::optional_field(&mut object, "baseUrl")?,
        default_model: crate::optional_field(&mut object, "defaultModel")?,
        enabled: crate::optional_field(&mut object, "enabled")?,
        api_key: crate::optional_field(&mut object, "apiKey")?,
        extra_headers: crate::optional_field(&mut object, "extraHeaders")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
