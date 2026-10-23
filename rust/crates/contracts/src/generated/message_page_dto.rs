// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessagePageDto {
    #[serde(rename = "items")]
    pub items: Vec<MessageDto>,
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub next_cursor: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(MessagePageDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        items: crate::required_field(&mut object, "items")?,
        next_cursor: crate::optional_field(&mut object, "nextCursor")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
