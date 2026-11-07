// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ContextUnitDtoKind {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "toolPair")]
    ToolPair,
}
crate::impl_json_via_serde!(ContextUnitDtoKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUnitDto {
    #[serde(rename = "kind")]
    pub kind: ContextUnitDtoKind,
    #[serde(rename = "text")]
    pub text: AiProviderKind,
    #[serde(rename = "citations")]
    pub citations: Vec<AiProviderKind>,
    #[serde(rename = "required")]
    pub required: bool,
}
crate::impl_json_deserialize!(ContextUnitDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        text: crate::required_field(&mut object, "text")?,
        citations: crate::required_field(&mut object, "citations")?,
        required: crate::required_field(&mut object, "required")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
