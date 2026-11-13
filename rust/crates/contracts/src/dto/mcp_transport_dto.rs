use crate::{McpHttpTransportDto, McpStdioTransportDto};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum McpTransportDto {
    Stdio(McpStdioTransportDto),
    Http(McpHttpTransportDto),
}
crate::impl_json_deserialize!(McpTransportDto, |value: Value| {
    if let Ok(member) = <McpStdioTransportDto as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Stdio(member));
    }
    if let Ok(member) = <McpHttpTransportDto as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Http(member));
    }
    Err(crate::json_error("invalid McpTransportDto"))
});
