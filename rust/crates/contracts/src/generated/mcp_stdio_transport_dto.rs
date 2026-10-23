// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum McpStdioTransportDtoKind {
    #[serde(rename = "stdio")]
    Stdio,
}
crate::impl_json_via_serde!(McpStdioTransportDtoKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpStdioTransportDto {
    #[serde(rename = "kind")]
    pub kind: McpStdioTransportDtoKind,
    #[serde(rename = "command")]
    pub command: AiProviderKind,
    #[serde(rename = "args", default, skip_serializing_if = "Optional::is_absent")]
    pub args: Optional<Vec<AiProviderKind>>,
    #[serde(rename = "env", default, skip_serializing_if = "Optional::is_absent")]
    pub env: Optional<BTreeMap<String, AiProviderKind>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(McpStdioTransportDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        command: crate::required_field(&mut object, "command")?,
        args: crate::optional_field(&mut object, "args")?,
        env: crate::optional_field(&mut object, "env")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
