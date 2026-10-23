// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolEventDto {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "runId")]
    pub run_id: AiProviderKind,
    #[serde(rename = "chatId")]
    pub chat_id: AiProviderKind,
    #[serde(rename = "toolCallId")]
    pub tool_call_id: AiProviderKind,
    #[serde(rename = "toolName")]
    pub tool_name: AiProviderKind,
    #[serde(rename = "status")]
    pub status: AiToolStatus,
    #[serde(
        rename = "argumentsJson",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub arguments_json: Optional<AiProviderKind>,
    #[serde(
        rename = "resultJson",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub result_json: Optional<AiProviderKind>,
    #[serde(
        rename = "modelResultJson",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub model_result_json: Optional<AiProviderKind>,
    #[serde(
        rename = "summary",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub summary: Optional<AiProviderKind>,
    #[serde(
        rename = "errorCode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub error_code: Optional<AiProviderKind>,
    #[serde(
        rename = "errorMessage",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub error_message: Optional<AiProviderKind>,
    #[serde(
        rename = "sources",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub sources: Optional<Vec<AiSourceRef>>,
    #[serde(
        rename = "warnings",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub warnings: Optional<Vec<AiProviderKind>>,
    #[serde(
        rename = "truncated",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub truncated: Optional<bool>,
    #[serde(rename = "createdAt")]
    pub created_at: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ToolEventDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        run_id: crate::required_field(&mut object, "runId")?,
        chat_id: crate::required_field(&mut object, "chatId")?,
        tool_call_id: crate::required_field(&mut object, "toolCallId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        status: crate::required_field(&mut object, "status")?,
        arguments_json: crate::optional_field(&mut object, "argumentsJson")?,
        result_json: crate::optional_field(&mut object, "resultJson")?,
        model_result_json: crate::optional_field(&mut object, "modelResultJson")?,
        summary: crate::optional_field(&mut object, "summary")?,
        error_code: crate::optional_field(&mut object, "errorCode")?,
        error_message: crate::optional_field(&mut object, "errorMessage")?,
        sources: crate::optional_field(&mut object, "sources")?,
        warnings: crate::optional_field(&mut object, "warnings")?,
        truncated: crate::optional_field(&mut object, "truncated")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
