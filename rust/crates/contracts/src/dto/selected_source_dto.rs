use crate::ContextUnitDto;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SelectedSourceDtoKind {
    #[serde(rename = "skill")]
    Skill,
    #[serde(rename = "resource")]
    Resource,
}
crate::impl_json_via_serde!(SelectedSourceDtoKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedSourceDto {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "kind")]
    pub kind: SelectedSourceDtoKind,
    #[serde(rename = "units")]
    pub units: Vec<ContextUnitDto>,
}
crate::impl_json_deserialize!(SelectedSourceDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        kind: crate::required_field(&mut object, "kind")?,
        units: crate::required_field(&mut object, "units")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
