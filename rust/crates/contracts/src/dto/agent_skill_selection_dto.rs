use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSkillSelectionDto {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "revision")]
    pub revision: String,
}
crate::impl_json_deserialize!(AgentSkillSelectionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        revision: crate::required_field(&mut object, "revision")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
