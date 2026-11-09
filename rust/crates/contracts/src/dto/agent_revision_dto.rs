use crate::{AgentDefinitionDto, AgentRevisionSourceDto};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentRevisionDto {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "revision", serialize_with = "crate::serialize_number")]
    pub revision: f64,
    #[serde(rename = "snapshot")]
    pub snapshot: AgentDefinitionDto,
    #[serde(rename = "source")]
    pub source: AgentRevisionSourceDto,
    #[serde(rename = "createdAt", serialize_with = "crate::serialize_number")]
    pub created_at: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AgentRevisionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        agent_id: crate::required_field(&mut object, "agentId")?,
        revision: crate::required_field(&mut object, "revision")?,
        snapshot: crate::required_field(&mut object, "snapshot")?,
        source: crate::required_field(&mut object, "source")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
