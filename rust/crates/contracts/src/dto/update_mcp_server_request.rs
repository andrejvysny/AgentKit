use crate::{McpTransportDto, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateMcpServerRequest {
    #[serde(rename = "alias", default, skip_serializing_if = "Optional::is_absent")]
    pub alias: Optional<String>,
    #[serde(
        rename = "transport",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub transport: Optional<McpTransportDto>,
    #[serde(
        rename = "secretRefs",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub secret_refs: Optional<BTreeMap<String, String>>,
    #[serde(
        rename = "enabled",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub enabled: Optional<bool>,
    #[serde(
        rename = "toolAliases",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_aliases: Optional<BTreeMap<String, String>>,
    #[serde(
        rename = "resilience",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub resilience: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(UpdateMcpServerRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        alias: crate::optional_field(&mut object, "alias")?,
        transport: crate::optional_field(&mut object, "transport")?,
        secret_refs: crate::optional_field(&mut object, "secretRefs")?,
        enabled: crate::optional_field(&mut object, "enabled")?,
        tool_aliases: crate::optional_field(&mut object, "toolAliases")?,
        resilience: crate::optional_field(&mut object, "resilience")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
