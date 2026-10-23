// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolDefinition {
    #[serde(rename = "name")]
    pub name: AiProviderKind,
    #[serde(rename = "version")]
    pub version: AiProviderKind,
    #[serde(rename = "effect")]
    pub effect: AiToolEffect,
    #[serde(rename = "capability")]
    pub capability: AiProviderKind,
    #[serde(rename = "description")]
    pub description: AiProviderKind,
    #[serde(rename = "inputSchema")]
    pub input_schema: AiJsonSchemaObject,
    #[serde(
        rename = "outputSchema",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub output_schema: Optional<AiJsonSchemaObject>,
    #[serde(
        rename = "timeoutMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub timeout_ms: Optional<f64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolDefinition, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        name: crate::required_field(&mut object, "name")?,
        version: crate::required_field(&mut object, "version")?,
        effect: crate::required_field(&mut object, "effect")?,
        capability: crate::required_field(&mut object, "capability")?,
        description: crate::required_field(&mut object, "description")?,
        input_schema: crate::required_field(&mut object, "inputSchema")?,
        output_schema: crate::optional_field(&mut object, "outputSchema")?,
        timeout_ms: crate::optional_field(&mut object, "timeoutMs")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
