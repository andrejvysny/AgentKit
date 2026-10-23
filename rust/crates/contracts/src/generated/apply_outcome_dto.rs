// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ApplyOutcomeDtoStatus {
    #[serde(rename = "applied")]
    Applied,
    #[serde(rename = "partial")]
    Partial,
    #[serde(rename = "failed")]
    Failed,
}
crate::impl_json_via_serde!(ApplyOutcomeDtoStatus);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ApplyOutcomeDtoFailedOpsItem {
    #[serde(rename = "opIndex", serialize_with = "crate::serialize_number")]
    pub op_index: f64,
    #[serde(rename = "error")]
    pub error: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ApplyOutcomeDtoFailedOpsItem, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        op_index: crate::required_field(&mut object, "opIndex")?,
        error: crate::required_field(&mut object, "error")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ApplyOutcomeDto {
    #[serde(rename = "status")]
    pub status: ApplyOutcomeDtoStatus,
    #[serde(rename = "appliedOps", serialize_with = "crate::serialize_number")]
    pub applied_ops: f64,
    #[serde(rename = "failedOps")]
    pub failed_ops: Vec<ApplyOutcomeDtoFailedOpsItem>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ApplyOutcomeDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        status: crate::required_field(&mut object, "status")?,
        applied_ops: crate::required_field(&mut object, "appliedOps")?,
        failed_ops: crate::required_field(&mut object, "failedOps")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
