// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiToolEnvelopeStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "partial")]
    Partial,
}
crate::impl_json_via_serde!(AiToolEnvelopeStatus);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolEnvelope {
    #[serde(
        rename = "modelContent",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub model_content: Optional<Vec<AiContentPart>>,
    #[serde(rename = "ok")]
    pub ok: bool,
    #[serde(rename = "status")]
    pub status: AiToolEnvelopeStatus,
    #[serde(
        rename = "summary",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub summary: Optional<AiProviderKind>,
    #[serde(rename = "warnings")]
    pub warnings: Vec<AiProviderKind>,
    #[serde(rename = "truncated")]
    pub truncated: bool,
    #[serde(rename = "data")]
    pub data: Value,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolEnvelope, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        model_content: crate::optional_field(&mut object, "modelContent")?,
        ok: crate::required_field(&mut object, "ok")?,
        status: crate::required_field(&mut object, "status")?,
        summary: crate::optional_field(&mut object, "summary")?,
        warnings: crate::required_field(&mut object, "warnings")?,
        truncated: crate::required_field(&mut object, "truncated")?,
        data: crate::required_field(&mut object, "data")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
