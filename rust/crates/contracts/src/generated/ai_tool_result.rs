// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiToolResultStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "partial")]
    Partial,
}
crate::impl_json_via_serde!(AiToolResultStatus);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolResult {
    #[serde(rename = "ok")]
    pub ok: bool,
    #[serde(rename = "data", default, skip_serializing_if = "Optional::is_absent")]
    pub data: Optional<Value>,
    #[serde(rename = "sources")]
    pub sources: Vec<AiSourceRef>,
    #[serde(rename = "warnings")]
    pub warnings: Vec<AiProviderKind>,
    #[serde(rename = "truncated")]
    pub truncated: bool,
    #[serde(rename = "limits")]
    pub limits: AiToolLimits,
    #[serde(
        rename = "modelData",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub model_data: Optional<Value>,
    #[serde(
        rename = "summary",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub summary: Optional<AiProviderKind>,
    #[serde(
        rename = "status",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub status: Optional<AiToolResultStatus>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolResult, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        ok: crate::required_field(&mut object, "ok")?,
        data: crate::optional_field(&mut object, "data")?,
        sources: crate::required_field(&mut object, "sources")?,
        warnings: crate::required_field(&mut object, "warnings")?,
        truncated: crate::required_field(&mut object, "truncated")?,
        limits: crate::required_field(&mut object, "limits")?,
        model_data: crate::optional_field(&mut object, "modelData")?,
        summary: crate::optional_field(&mut object, "summary")?,
        status: crate::optional_field(&mut object, "status")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
