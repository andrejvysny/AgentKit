// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunDto {
    #[serde(rename = "runId")]
    pub run_id: AiProviderKind,
    #[serde(rename = "chatId")]
    pub chat_id: AiProviderKind,
    #[serde(rename = "scopeId")]
    pub scope_id: AiProviderKind,
    #[serde(rename = "status")]
    pub status: RunStatusDto,
    #[serde(rename = "createdAt")]
    pub created_at: AiProviderKind,
    #[serde(
        rename = "startedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub started_at: Optional<AiProviderKind>,
    #[serde(
        rename = "finishedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub finished_at: Optional<AiProviderKind>,
    #[serde(rename = "error", default, skip_serializing_if = "Optional::is_absent")]
    pub error: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(RunDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        run_id: crate::required_field(&mut object, "runId")?,
        chat_id: crate::required_field(&mut object, "chatId")?,
        scope_id: crate::required_field(&mut object, "scopeId")?,
        status: crate::required_field(&mut object, "status")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        started_at: crate::optional_field(&mut object, "startedAt")?,
        finished_at: crate::optional_field(&mut object, "finishedAt")?,
        error: crate::optional_field(&mut object, "error")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
