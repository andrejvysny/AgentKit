use crate::{AgentTaskStatusDto, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentTaskDto {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "agentRevision", serialize_with = "crate::serialize_number")]
    pub agent_revision: f64,
    #[serde(rename = "admissionKey")]
    pub admission_key: String,
    #[serde(rename = "kind")]
    pub kind: String,
    #[serde(rename = "status")]
    pub status: AgentTaskStatusDto,
    #[serde(rename = "priority", serialize_with = "crate::serialize_number")]
    pub priority: f64,
    #[serde(rename = "enqueuedAt", serialize_with = "crate::serialize_number")]
    pub enqueued_at: f64,
    #[serde(rename = "availableAt", serialize_with = "crate::serialize_number")]
    pub available_at: f64,
    #[serde(
        rename = "startedAt",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub started_at: Optional<f64>,
    #[serde(
        rename = "finishedAt",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub finished_at: Optional<f64>,
    #[serde(rename = "error", default, skip_serializing_if = "Optional::is_absent")]
    pub error: Optional<String>,
    #[serde(rename = "attemptCount", serialize_with = "crate::serialize_number")]
    pub attempt_count: f64,
    #[serde(rename = "cancelRequested")]
    pub cancel_requested: bool,
    #[serde(rename = "dependsOn")]
    pub depends_on: Vec<String>,
    #[serde(
        rename = "parentTaskId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub parent_task_id: Optional<String>,
    #[serde(
        rename = "runProfileDigest",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub run_profile_digest: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AgentTaskDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        task_id: crate::required_field(&mut object, "taskId")?,
        agent_id: crate::required_field(&mut object, "agentId")?,
        agent_revision: crate::required_field(&mut object, "agentRevision")?,
        admission_key: crate::required_field(&mut object, "admissionKey")?,
        kind: crate::required_field(&mut object, "kind")?,
        status: crate::required_field(&mut object, "status")?,
        priority: crate::required_field(&mut object, "priority")?,
        enqueued_at: crate::required_field(&mut object, "enqueuedAt")?,
        available_at: crate::required_field(&mut object, "availableAt")?,
        started_at: crate::optional_field(&mut object, "startedAt")?,
        finished_at: crate::optional_field(&mut object, "finishedAt")?,
        error: crate::optional_field(&mut object, "error")?,
        attempt_count: crate::required_field(&mut object, "attemptCount")?,
        cancel_requested: crate::required_field(&mut object, "cancelRequested")?,
        depends_on: crate::required_field(&mut object, "dependsOn")?,
        parent_task_id: crate::optional_field(&mut object, "parentTaskId")?,
        run_profile_digest: crate::optional_field(&mut object, "runProfileDigest")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
