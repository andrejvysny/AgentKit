// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiChatMessage {
    #[serde(rename = "role")]
    pub role: AiChatRole,
    #[serde(rename = "content")]
    pub content: AiMessageContent,
    #[serde(rename = "name", default, skip_serializing_if = "Optional::is_absent")]
    pub name: Optional<AiProviderKind>,
    #[serde(
        rename = "toolCallId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_call_id: Optional<AiProviderKind>,
    #[serde(
        rename = "toolCalls",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_calls: Optional<Vec<AiToolCall>>,
    #[serde(
        rename = "metadata",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub metadata: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiChatMessage, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        role: crate::required_field(&mut object, "role")?,
        content: crate::required_field(&mut object, "content")?,
        name: crate::optional_field(&mut object, "name")?,
        tool_call_id: crate::optional_field(&mut object, "toolCallId")?,
        tool_calls: crate::optional_field(&mut object, "toolCalls")?,
        metadata: crate::optional_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
