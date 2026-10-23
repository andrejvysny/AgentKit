// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MessageDtoRole {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "tool")]
    Tool,
    #[serde(rename = "system")]
    System,
}
crate::impl_json_via_serde!(MessageDtoRole);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessageDto {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "chatId")]
    pub chat_id: AiProviderKind,
    #[serde(rename = "runId", default, skip_serializing_if = "Optional::is_absent")]
    pub run_id: Optional<AiProviderKind>,
    #[serde(rename = "role")]
    pub role: MessageDtoRole,
    #[serde(rename = "content")]
    pub content: AiMessageContent,
    #[serde(
        rename = "toolCalls",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_calls: Optional<Vec<AiToolCall>>,
    #[serde(
        rename = "toolCallId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_call_id: Optional<AiProviderKind>,
    #[serde(
        rename = "parentMessageId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub parent_message_id: Optional<AiProviderKind>,
    #[serde(
        rename = "branchIndex",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub branch_index: Optional<f64>,
    #[serde(
        rename = "active",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub active: Optional<bool>,
    #[serde(rename = "metadata")]
    pub metadata: BTreeMap<String, Value>,
    #[serde(rename = "createdAt")]
    pub created_at: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(MessageDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        chat_id: crate::required_field(&mut object, "chatId")?,
        run_id: crate::optional_field(&mut object, "runId")?,
        role: crate::required_field(&mut object, "role")?,
        content: crate::required_field(&mut object, "content")?,
        tool_calls: crate::optional_field(&mut object, "toolCalls")?,
        tool_call_id: crate::optional_field(&mut object, "toolCallId")?,
        parent_message_id: crate::optional_field(&mut object, "parentMessageId")?,
        branch_index: crate::optional_field(&mut object, "branchIndex")?,
        active: crate::optional_field(&mut object, "active")?,
        metadata: crate::required_field(&mut object, "metadata")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
