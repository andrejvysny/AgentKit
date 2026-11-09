use crate::{Nullable, Optional};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAgentModelRequest {
    #[serde(
        rename = "expectedRevision",
        serialize_with = "crate::serialize_number"
    )]
    pub expected_revision: f64,
    #[serde(rename = "model", default, skip_serializing_if = "Optional::is_absent")]
    pub model: Optional<Nullable<String>>,
    #[serde(
        rename = "reasoningEffort",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reasoning_effort: Optional<Nullable<String>>,
}
crate::impl_json_deserialize!(SetAgentModelRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        expected_revision: crate::required_field(&mut object, "expectedRevision")?,
        model: crate::optional_field(&mut object, "model")?,
        reasoning_effort: crate::optional_field(&mut object, "reasoningEffort")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
