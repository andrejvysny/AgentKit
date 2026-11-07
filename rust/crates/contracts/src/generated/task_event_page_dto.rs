// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskEventPageDto {
    #[serde(rename = "events")]
    pub events: Vec<TaskEventEnvelope>,
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub next_cursor: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(TaskEventPageDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        events: crate::required_field(&mut object, "events")?,
        next_cursor: crate::optional_field(&mut object, "nextCursor")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
