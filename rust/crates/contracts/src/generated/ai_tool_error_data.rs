// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolErrorData {
    #[serde(rename = "errorCode")]
    pub error_code: AiProviderKind,
    #[serde(rename = "errorMessage")]
    pub error_message: AiProviderKind,
    #[serde(rename = "phase", default, skip_serializing_if = "Optional::is_absent")]
    pub phase: Optional<AiToolErrorPhase>,
    #[serde(
        rename = "retryable",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub retryable: Optional<bool>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolErrorData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        error_code: crate::required_field(&mut object, "errorCode")?,
        error_message: crate::required_field(&mut object, "errorMessage")?,
        phase: crate::optional_field(&mut object, "phase")?,
        retryable: crate::optional_field(&mut object, "retryable")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
