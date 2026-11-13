use crate::{AgentBudgetsDto, Optional};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TaskProfileOverridesDtoTruncationMode {
    #[serde(rename = "explicit")]
    Explicit,
}
crate::impl_json_via_serde!(TaskProfileOverridesDtoTruncationMode);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskProfileOverridesDtoTruncation {
    #[serde(rename = "mode")]
    pub mode: TaskProfileOverridesDtoTruncationMode,
}
crate::impl_json_deserialize!(TaskProfileOverridesDtoTruncation, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        mode: crate::required_field(&mut object, "mode")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskProfileOverridesDto {
    #[serde(
        rename = "providerId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub provider_id: Optional<String>,
    #[serde(rename = "model", default, skip_serializing_if = "Optional::is_absent")]
    pub model: Optional<String>,
    #[serde(rename = "tools", default, skip_serializing_if = "Optional::is_absent")]
    pub tools: Optional<Vec<String>>,
    #[serde(
        rename = "requiresVision",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub requires_vision: Optional<bool>,
    #[serde(
        rename = "requiresContinuation",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub requires_continuation: Optional<bool>,
    #[serde(
        rename = "permissionMode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub permission_mode: Optional<String>,
    #[serde(
        rename = "budgets",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub budgets: Optional<AgentBudgetsDto>,
    #[serde(
        rename = "truncation",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub truncation: Optional<TaskProfileOverridesDtoTruncation>,
}
crate::impl_json_deserialize!(TaskProfileOverridesDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        provider_id: crate::optional_field(&mut object, "providerId")?,
        model: crate::optional_field(&mut object, "model")?,
        tools: crate::optional_field(&mut object, "tools")?,
        requires_vision: crate::optional_field(&mut object, "requiresVision")?,
        requires_continuation: crate::optional_field(&mut object, "requiresContinuation")?,
        permission_mode: crate::optional_field(&mut object, "permissionMode")?,
        budgets: crate::optional_field(&mut object, "budgets")?,
        truncation: crate::optional_field(&mut object, "truncation")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
