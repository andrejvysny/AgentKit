use crate::{
    AgentTaskDto, AiSourceRef, Optional, TaskEventUsageTotalsDto, TaskProjectionApprovalDto,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentTaskProjectionDto {
    #[serde(rename = "task")]
    pub task: AgentTaskDto,
    #[serde(
        rename = "attemptId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub attempt_id: Optional<String>,
    #[serde(
        rename = "lastSeq",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub last_seq: Optional<f64>,
    #[serde(rename = "eventCount", serialize_with = "crate::serialize_number")]
    pub event_count: f64,
    #[serde(
        rename = "approval",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub approval: Optional<TaskProjectionApprovalDto>,
    #[serde(rename = "usage")]
    pub usage: TaskEventUsageTotalsDto,
    #[serde(rename = "sourceRefs")]
    pub source_refs: Vec<AiSourceRef>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AgentTaskProjectionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        task: crate::required_field(&mut object, "task")?,
        attempt_id: crate::optional_field(&mut object, "attemptId")?,
        last_seq: crate::optional_field(&mut object, "lastSeq")?,
        event_count: crate::required_field(&mut object, "eventCount")?,
        approval: crate::optional_field(&mut object, "approval")?,
        usage: crate::required_field(&mut object, "usage")?,
        source_refs: crate::required_field(&mut object, "sourceRefs")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
