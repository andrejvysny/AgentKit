// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunContentSelectionTemplate {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "revision")]
    pub revision: AiProviderKind,
    #[serde(rename = "arguments")]
    pub arguments: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(RunContentSelectionTemplate, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        revision: crate::required_field(&mut object, "revision")?,
        arguments: crate::required_field(&mut object, "arguments")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunContentSelectionSkillsItem {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "revision")]
    pub revision: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(RunContentSelectionSkillsItem, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        revision: crate::required_field(&mut object, "revision")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunContentSelection {
    #[serde(
        rename = "template",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub template: Optional<RunContentSelectionTemplate>,
    #[serde(
        rename = "skills",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub skills: Optional<Vec<RunContentSelectionSkillsItem>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(RunContentSelection, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        template: crate::optional_field(&mut object, "template")?,
        skills: crate::optional_field(&mut object, "skills")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
