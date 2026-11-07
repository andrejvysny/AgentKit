// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentDto {
    #[serde(rename = "agentId")]
    pub agent_id: AiProviderKind,
    #[serde(rename = "scope")]
    pub scope: AiProviderKind,
    #[serde(rename = "ownerId")]
    pub owner_id: AiProviderKind,
    #[serde(rename = "lifecycle")]
    pub lifecycle: AgentLifecycleDto,
    #[serde(rename = "currentRevision", serialize_with = "crate::serialize_number")]
    pub current_revision: f64,
    #[serde(rename = "definition")]
    pub definition: AgentDefinitionDto,
    #[serde(rename = "createdAt", serialize_with = "crate::serialize_number")]
    pub created_at: f64,
    #[serde(rename = "updatedAt", serialize_with = "crate::serialize_number")]
    pub updated_at: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AgentDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        agent_id: crate::required_field(&mut object, "agentId")?,
        scope: crate::required_field(&mut object, "scope")?,
        owner_id: crate::required_field(&mut object, "ownerId")?,
        lifecycle: crate::required_field(&mut object, "lifecycle")?,
        current_revision: crate::required_field(&mut object, "currentRevision")?,
        definition: crate::required_field(&mut object, "definition")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        updated_at: crate::required_field(&mut object, "updatedAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
