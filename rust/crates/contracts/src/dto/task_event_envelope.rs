use crate::{AgentTaskStatusDto, AiSourceRef, Optional, TaskEventApproval, TaskEventUsage};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskEventEnvelope {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(rename = "seq", serialize_with = "crate::serialize_number")]
    pub seq: f64,
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "timestamp")]
    pub timestamp: String,
    #[serde(rename = "contractVersion")]
    pub contract_version: String,
    #[serde(
        rename = "attemptId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub attempt_id: Optional<String>,
    #[serde(
        rename = "agentId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub agent_id: Optional<String>,
    #[serde(
        rename = "taskId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub task_id: Optional<String>,
    #[serde(
        rename = "definitionRevision",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub definition_revision: Optional<f64>,
    #[serde(
        rename = "status",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub status: Optional<AgentTaskStatusDto>,
    #[serde(
        rename = "approval",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub approval: Optional<TaskEventApproval>,
    #[serde(rename = "usage", default, skip_serializing_if = "Optional::is_absent")]
    pub usage: Optional<TaskEventUsage>,
    #[serde(
        rename = "sourceRefs",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub source_refs: Optional<Vec<AiSourceRef>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(TaskEventEnvelope, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        seq: crate::required_field(&mut object, "seq")?,
        event_id: crate::required_field(&mut object, "eventId")?,
        timestamp: crate::required_field(&mut object, "timestamp")?,
        contract_version: crate::required_field(&mut object, "contractVersion")?,
        attempt_id: crate::optional_field(&mut object, "attemptId")?,
        agent_id: crate::optional_field(&mut object, "agentId")?,
        task_id: crate::optional_field(&mut object, "taskId")?,
        definition_revision: crate::optional_field(&mut object, "definitionRevision")?,
        status: crate::optional_field(&mut object, "status")?,
        approval: crate::optional_field(&mut object, "approval")?,
        usage: crate::optional_field(&mut object, "usage")?,
        source_refs: crate::optional_field(&mut object, "sourceRefs")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
