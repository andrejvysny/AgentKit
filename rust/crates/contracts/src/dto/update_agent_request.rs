use crate::AgentDefinitionDto;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgentRequest {
    #[serde(
        rename = "expectedRevision",
        serialize_with = "crate::serialize_number"
    )]
    pub expected_revision: f64,
    #[serde(rename = "definition")]
    pub definition: AgentDefinitionDto,
}
crate::impl_json_deserialize!(UpdateAgentRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        expected_revision: crate::required_field(&mut object, "expectedRevision")?,
        definition: crate::required_field(&mut object, "definition")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
