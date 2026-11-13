use crate::{Optional, RunStatusDto};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunDto {
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "scopeId")]
    pub scope_id: String,
    #[serde(rename = "status")]
    pub status: RunStatusDto,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(
        rename = "startedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub started_at: Optional<String>,
    #[serde(
        rename = "finishedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub finished_at: Optional<String>,
    #[serde(rename = "error", default, skip_serializing_if = "Optional::is_absent")]
    pub error: Optional<String>,
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
