// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiImagePartType {
    #[serde(rename = "image")]
    Image,
}
crate::impl_json_via_serde!(AiImagePartType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiImagePartDetail {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "high")]
    High,
}
crate::impl_json_via_serde!(AiImagePartDetail);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiImagePart {
    #[serde(rename = "type")]
    pub type_: AiImagePartType,
    #[serde(rename = "source")]
    pub source: AiImageSource,
    #[serde(
        rename = "detail",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub detail: Optional<AiImagePartDetail>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiImagePart, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        source: crate::required_field(&mut object, "source")?,
        detail: crate::optional_field(&mut object, "detail")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
