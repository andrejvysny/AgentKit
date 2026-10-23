// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiContextBinding {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "kind")]
    pub kind: AiProviderKind,
    #[serde(rename = "refId")]
    pub ref_id: AiProviderKind,
    #[serde(rename = "label")]
    pub label: AiProviderKind,
    #[serde(rename = "role")]
    pub role: AiContextBindingRole,
    #[serde(rename = "status")]
    pub status: AiContextBindingStatus,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiContextBinding, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        kind: crate::required_field(&mut object, "kind")?,
        ref_id: crate::required_field(&mut object, "refId")?,
        label: crate::required_field(&mut object, "label")?,
        role: crate::required_field(&mut object, "role")?,
        status: crate::required_field(&mut object, "status")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
