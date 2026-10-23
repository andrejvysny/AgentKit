// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegenerateMessageRequest {
    #[serde(rename = "model", default, skip_serializing_if = "Optional::is_absent")]
    pub model: Optional<AiProviderKind>,
    #[serde(
        rename = "providerId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub provider_id: Optional<AiProviderKind>,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(RegenerateMessageRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        model: crate::optional_field(&mut object, "model")?,
        provider_id: crate::optional_field(&mut object, "providerId")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
