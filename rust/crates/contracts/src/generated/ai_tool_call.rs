// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolCall {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "name")]
    pub name: AiProviderKind,
    #[serde(rename = "argumentsJson")]
    pub arguments_json: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolCall, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        name: crate::required_field(&mut object, "name")?,
        arguments_json: crate::required_field(&mut object, "argumentsJson")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
