// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChatDto {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(
        rename = "activeRunId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub active_run_id: Optional<Nullable<AiProviderKind>>,
    #[serde(rename = "title", default, skip_serializing_if = "Optional::is_absent")]
    pub title: Optional<AiProviderKind>,
    #[serde(rename = "createdAt")]
    pub created_at: AiProviderKind,
    #[serde(rename = "updatedAt")]
    pub updated_at: AiProviderKind,
    #[serde(rename = "archived")]
    pub archived: bool,
    #[serde(rename = "metadata")]
    pub metadata: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ChatDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        active_run_id: crate::optional_field(&mut object, "activeRunId")?,
        title: crate::optional_field(&mut object, "title")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        updated_at: crate::required_field(&mut object, "updatedAt")?,
        archived: crate::required_field(&mut object, "archived")?,
        metadata: crate::required_field(&mut object, "metadata")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
