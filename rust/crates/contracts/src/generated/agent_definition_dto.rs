// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefinitionDto {
    #[serde(rename = "name")]
    pub name: AiProviderKind,
    #[serde(
        rename = "avatar",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub avatar: Optional<AiProviderKind>,
    #[serde(rename = "purpose")]
    pub purpose: AiProviderKind,
    #[serde(rename = "instructions")]
    pub instructions: AiProviderKind,
    #[serde(rename = "resources")]
    pub resources: Vec<AiProviderKind>,
    #[serde(rename = "skills")]
    pub skills: Vec<AgentSkillSelectionDto>,
    #[serde(rename = "model", default, skip_serializing_if = "Optional::is_absent")]
    pub model: Optional<AiProviderKind>,
    #[serde(
        rename = "reasoningEffort",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_effort: Optional<AiProviderKind>,
    #[serde(rename = "permissionMode")]
    pub permission_mode: AiProviderKind,
    #[serde(rename = "allowedTools")]
    pub allowed_tools: Vec<AiProviderKind>,
    #[serde(rename = "budgets")]
    pub budgets: AgentBudgetsDto,
    #[serde(rename = "secretRefs")]
    pub secret_refs: Vec<AiProviderKind>,
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
