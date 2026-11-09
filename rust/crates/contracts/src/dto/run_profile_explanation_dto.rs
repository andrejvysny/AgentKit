use crate::Optional;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunProfileExplanationDto {
    #[serde(rename = "taskId")]
    pub task_id: String,
    #[serde(
        rename = "digest",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub digest: Optional<String>,
    #[serde(
        rename = "profile",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub profile: Optional<BTreeMap<String, Value>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(RunProfileExplanationDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        task_id: crate::required_field(&mut object, "taskId")?,
        digest: crate::optional_field(&mut object, "digest")?,
        profile: crate::optional_field(&mut object, "profile")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
