use crate::RiskLevelDto;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GrantAllowanceRequest {
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "proposalKind")]
    pub proposal_kind: String,
    #[serde(rename = "maxRisk")]
    pub max_risk: RiskLevelDto,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(GrantAllowanceRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        tool_name: crate::required_field(&mut object, "toolName")?,
        proposal_kind: crate::required_field(&mut object, "proposalKind")?,
        max_risk: crate::required_field(&mut object, "maxRisk")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
