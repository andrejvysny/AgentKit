use crate::{AgentBudgetsDto, AgentSkillSelectionDto, Optional};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinitionDto {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(
        rename = "avatar",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub avatar: Optional<String>,
    #[serde(rename = "purpose")]
    pub purpose: String,
    #[serde(rename = "instructions")]
    pub instructions: String,
    #[serde(rename = "resources")]
    pub resources: Vec<String>,
    #[serde(rename = "skills")]
    pub skills: Vec<AgentSkillSelectionDto>,
    #[serde(rename = "model", default, skip_serializing_if = "Optional::is_absent")]
    pub model: Optional<String>,
    #[serde(
        rename = "reasoningEffort",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_effort: Optional<String>,
    #[serde(rename = "permissionMode")]
    pub permission_mode: String,
    #[serde(rename = "allowedTools")]
    pub allowed_tools: Vec<String>,
    #[serde(rename = "budgets")]
    pub budgets: AgentBudgetsDto,
    #[serde(rename = "secretRefs")]
    pub secret_refs: Vec<String>,
}
crate::impl_json_deserialize!(AgentDefinitionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        name: crate::required_field(&mut object, "name")?,
        avatar: crate::optional_field(&mut object, "avatar")?,
        purpose: crate::required_field(&mut object, "purpose")?,
        instructions: crate::required_field(&mut object, "instructions")?,
        resources: crate::required_field(&mut object, "resources")?,
        skills: crate::required_field(&mut object, "skills")?,
        model: crate::optional_field(&mut object, "model")?,
        reasoning_effort: crate::optional_field(&mut object, "reasoningEffort")?,
        permission_mode: crate::required_field(&mut object, "permissionMode")?,
        allowed_tools: crate::required_field(&mut object, "allowedTools")?,
        budgets: crate::required_field(&mut object, "budgets")?,
        secret_refs: crate::required_field(&mut object, "secretRefs")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
