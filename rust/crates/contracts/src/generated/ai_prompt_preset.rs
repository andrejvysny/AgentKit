// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiPromptPreset {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "label")]
    pub label: AiProviderKind,
    #[serde(rename = "description")]
    pub description: AiProviderKind,
    #[serde(rename = "systemText")]
    pub system_text: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiPromptPreset, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        label: crate::required_field(&mut object, "label")?,
        description: crate::required_field(&mut object, "description")?,
        system_text: crate::required_field(&mut object, "systemText")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
