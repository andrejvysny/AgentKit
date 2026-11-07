// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiCitation {
    #[serde(rename = "url")]
    pub url: AiProviderKind,
    #[serde(rename = "title", default, skip_serializing_if = "Optional::is_absent")]
    pub title: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiCitation, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        url: crate::required_field(&mut object, "url")?,
        title: crate::optional_field(&mut object, "title")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
