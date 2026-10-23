// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiPromptContextBlock {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "title")]
    pub title: AiProviderKind,
    #[serde(rename = "content")]
    pub content: AiProviderKind,
    #[serde(rename = "priority", serialize_with = "crate::serialize_number")]
    pub priority: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiPromptContextBlock, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        title: crate::required_field(&mut object, "title")?,
        content: crate::required_field(&mut object, "content")?,
        priority: crate::required_field(&mut object, "priority")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
