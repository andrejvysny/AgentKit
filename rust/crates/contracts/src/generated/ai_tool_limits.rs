// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolLimits {
    #[serde(rename = "profile")]
    pub profile: AiContextSizePreference,
    #[serde(rename = "maxBytes", serialize_with = "crate::serialize_number")]
    pub max_bytes: f64,
    #[serde(
        rename = "maxItems",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_items: Optional<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolLimits, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        profile: crate::required_field(&mut object, "profile")?,
        max_bytes: crate::required_field(&mut object, "maxBytes")?,
        max_items: crate::optional_field(&mut object, "maxItems")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
