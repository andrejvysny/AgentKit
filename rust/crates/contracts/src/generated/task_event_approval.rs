// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TaskEventApprovalState {
    #[serde(rename = "requested")]
    Requested,
    #[serde(rename = "granted")]
    Granted,
    #[serde(rename = "denied")]
    Denied,
}
crate::impl_json_via_serde!(TaskEventApprovalState);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskEventApproval {
    #[serde(rename = "state")]
    pub state: TaskEventApprovalState,
    #[serde(
        rename = "approvalId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub approval_id: Optional<AiProviderKind>,
    #[serde(
        rename = "toolName",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_name: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(TaskEventApproval, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        state: crate::required_field(&mut object, "state")?,
        approval_id: crate::optional_field(&mut object, "approvalId")?,
        tool_name: crate::optional_field(&mut object, "toolName")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
