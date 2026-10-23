// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum McpHttpTransportDtoKind {
    #[serde(rename = "http")]
    Http,
}
crate::impl_json_via_serde!(McpHttpTransportDtoKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpHttpTransportDto {
    #[serde(rename = "kind")]
    pub kind: McpHttpTransportDtoKind,
    #[serde(rename = "url")]
    pub url: AiProviderKind,
    #[serde(
        rename = "headers",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub headers: Optional<BTreeMap<String, AiProviderKind>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(McpHttpTransportDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        url: crate::required_field(&mut object, "url")?,
        headers: crate::optional_field(&mut object, "headers")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
