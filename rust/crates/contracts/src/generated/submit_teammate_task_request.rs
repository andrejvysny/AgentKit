// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitTeammateTaskRequest {
    #[serde(
        rename = "taskId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub task_id: Optional<AiProviderKind>,
    #[serde(rename = "admissionKey")]
    pub admission_key: AiProviderKind,
    #[serde(rename = "kind")]
    pub kind: AiProviderKind,
    #[serde(rename = "payload")]
    pub payload: BTreeMap<String, Value>,
    #[serde(
        rename = "priority",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub priority: Optional<f64>,
    #[serde(
        rename = "dependsOn",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub depends_on: Optional<Vec<AiProviderKind>>,
    #[serde(
        rename = "parentTaskId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub parent_task_id: Optional<AiProviderKind>,
    #[serde(
        rename = "taskProfile",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub task_profile: Optional<TaskProfileOverridesDto>,
    #[serde(
        rename = "projectSources",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub project_sources: Optional<Vec<SelectedSourceDto>>,
    #[serde(
        rename = "approvals",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub approvals: Optional<Vec<ApprovalGrantRefDto>>,
}
crate::impl_json_deserialize!(SubmitTeammateTaskRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        task_id: crate::optional_field(&mut object, "taskId")?,
        admission_key: crate::required_field(&mut object, "admissionKey")?,
        kind: crate::required_field(&mut object, "kind")?,
        payload: crate::required_field(&mut object, "payload")?,
        priority: crate::optional_field(&mut object, "priority")?,
        depends_on: crate::optional_field(&mut object, "dependsOn")?,
        parent_task_id: crate::optional_field(&mut object, "parentTaskId")?,
        task_profile: crate::optional_field(&mut object, "taskProfile")?,
        project_sources: crate::optional_field(&mut object, "projectSources")?,
        approvals: crate::optional_field(&mut object, "approvals")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
