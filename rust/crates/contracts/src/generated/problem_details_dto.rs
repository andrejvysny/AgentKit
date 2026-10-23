// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProblemDetailsDto {
    #[serde(rename = "type")]
    pub type_: AiProviderKind,
    #[serde(rename = "title")]
    pub title: AiProviderKind,
    #[serde(rename = "status", serialize_with = "crate::serialize_number")]
    pub status: f64,
    #[serde(
        rename = "detail",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub detail: Optional<AiProviderKind>,
    #[serde(
        rename = "instance",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub instance: Optional<AiProviderKind>,
    #[serde(rename = "code", default, skip_serializing_if = "Optional::is_absent")]
    pub code: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ProblemDetailsDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        title: crate::required_field(&mut object, "title")?,
        status: crate::required_field(&mut object, "status")?,
        detail: crate::optional_field(&mut object, "detail")?,
        instance: crate::optional_field(&mut object, "instance")?,
        code: crate::optional_field(&mut object, "code")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
