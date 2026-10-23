// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiSourceRef {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "kind")]
    pub kind: AiProviderKind,
    #[serde(rename = "label")]
    pub label: AiProviderKind,
    #[serde(rename = "refId", default, skip_serializing_if = "Optional::is_absent")]
    pub ref_id: Optional<AiProviderKind>,
    #[serde(rename = "path", default, skip_serializing_if = "Optional::is_absent")]
    pub path: Optional<AiProviderKind>,
    #[serde(
        rename = "excerpt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub excerpt: Optional<AiProviderKind>,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiSourceRef, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        kind: crate::required_field(&mut object, "kind")?,
        label: crate::required_field(&mut object, "label")?,
        ref_id: crate::optional_field(&mut object, "refId")?,
        path: crate::optional_field(&mut object, "path")?,
        excerpt: crate::optional_field(&mut object, "excerpt")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
