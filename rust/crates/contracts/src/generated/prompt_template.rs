// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PromptTemplateMessagesItemRole {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
}
crate::impl_json_via_serde!(PromptTemplateMessagesItemRole);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PromptTemplateMessagesItem {
    #[serde(rename = "role")]
    pub role: PromptTemplateMessagesItemRole,
    #[serde(rename = "content")]
    pub content: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(PromptTemplateMessagesItem, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        role: crate::required_field(&mut object, "role")?,
        content: crate::required_field(&mut object, "content")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PromptTemplateOriginKind {
    #[serde(rename = "application")]
    Application,
    #[serde(rename = "mcp")]
    Mcp,
}
crate::impl_json_via_serde!(PromptTemplateOriginKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PromptTemplateOrigin {
    #[serde(rename = "kind")]
    pub kind: PromptTemplateOriginKind,
    #[serde(rename = "source")]
    pub source: AiProviderKind,
    #[serde(rename = "trusted")]
    pub trusted: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(PromptTemplateOrigin, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        source: crate::required_field(&mut object, "source")?,
        trusted: crate::required_field(&mut object, "trusted")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PromptTemplate {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "revision")]
    pub revision: AiProviderKind,
    #[serde(rename = "description")]
    pub description: AiProviderKind,
    #[serde(rename = "argumentsSchema")]
    pub arguments_schema: AiJsonSchemaObject,
    #[serde(rename = "messages")]
    pub messages: Vec<PromptTemplateMessagesItem>,
    #[serde(
        rename = "origin",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub origin: Optional<PromptTemplateOrigin>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(PromptTemplate, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        revision: crate::required_field(&mut object, "revision")?,
        description: crate::required_field(&mut object, "description")?,
        arguments_schema: crate::required_field(&mut object, "argumentsSchema")?,
        messages: crate::required_field(&mut object, "messages")?,
        origin: crate::optional_field(&mut object, "origin")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
