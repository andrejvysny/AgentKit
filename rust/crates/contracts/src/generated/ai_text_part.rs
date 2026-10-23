// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiTextPartType {
    #[serde(rename = "text")]
    Text,
}
crate::impl_json_via_serde!(AiTextPartType);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiTextPart {
    #[serde(rename = "type")]
    pub type_: AiTextPartType,
    #[serde(rename = "text")]
    pub text: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiTextPart, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        text: crate::required_field(&mut object, "text")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
