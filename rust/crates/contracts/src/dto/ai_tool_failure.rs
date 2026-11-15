use crate::{AiToolOutcomeClass, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The one failure shape every tool shares (TU-10). Rendered to the model as
/// the message, then "Why: <cause>", then "Next: <next_step>".
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolFailure {
    #[serde(rename = "code")]
    pub code: String,
    #[serde(rename = "class")]
    pub class: AiToolOutcomeClass,
    #[serde(rename = "message")]
    pub message: String,
    #[serde(rename = "cause", default, skip_serializing_if = "Optional::is_absent")]
    pub cause: Optional<String>,
    #[serde(rename = "nextStep")]
    pub next_step: String,
    #[serde(rename = "retryable")]
    pub retryable: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolFailure, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        code: crate::required_field(&mut object, "code")?,
        class: crate::required_field(&mut object, "class")?,
        message: crate::required_field(&mut object, "message")?,
        cause: crate::optional_field(&mut object, "cause")?,
        next_step: crate::required_field(&mut object, "nextStep")?,
        retryable: crate::required_field(&mut object, "retryable")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
