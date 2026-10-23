// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskEventEnvelope {
    #[serde(rename = "type")]
    pub type_: AiProviderKind,
    #[serde(rename = "seq", serialize_with = "crate::serialize_number")]
    pub seq: f64,
    #[serde(rename = "eventId")]
    pub event_id: AiProviderKind,
    #[serde(rename = "timestamp")]
    pub timestamp: AiProviderKind,
    #[serde(rename = "contractVersion")]
    pub contract_version: AiProviderKind,
    #[serde(
        rename = "attemptId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub attempt_id: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(TaskEventEnvelope, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        seq: crate::required_field(&mut object, "seq")?,
        event_id: crate::required_field(&mut object, "eventId")?,
        timestamp: crate::required_field(&mut object, "timestamp")?,
        contract_version: crate::required_field(&mut object, "contractVersion")?,
        attempt_id: crate::optional_field(&mut object, "attemptId")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
