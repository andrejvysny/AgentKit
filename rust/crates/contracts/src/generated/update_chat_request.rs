// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateChatRequest {
    #[serde(rename = "title", default, skip_serializing_if = "Optional::is_absent")]
    pub title: Optional<AiProviderKind>,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(
        rename = "archived",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub archived: Optional<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(UpdateChatRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        title: crate::optional_field(&mut object, "title")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        archived: crate::optional_field(&mut object, "archived")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
