use crate::Optional;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TaskProjectionApprovalDtoState {
    #[serde(rename = "requested")]
    Requested,
    #[serde(rename = "granted")]
    Granted,
    #[serde(rename = "denied")]
    Denied,
}
crate::impl_json_via_serde!(TaskProjectionApprovalDtoState);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskProjectionApprovalDto {
    #[serde(rename = "state")]
    pub state: TaskProjectionApprovalDtoState,
    #[serde(
        rename = "approvalId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub approval_id: Optional<String>,
    #[serde(
        rename = "toolName",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub tool_name: Optional<String>,
    #[serde(rename = "seq", serialize_with = "crate::serialize_number")]
    pub seq: f64,
}
crate::impl_json_deserialize!(TaskProjectionApprovalDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        state: crate::required_field(&mut object, "state")?,
        approval_id: crate::optional_field(&mut object, "approvalId")?,
        tool_name: crate::optional_field(&mut object, "toolName")?,
        seq: crate::required_field(&mut object, "seq")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
