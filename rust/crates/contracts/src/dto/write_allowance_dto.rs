use crate::RiskLevelDto;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WriteAllowanceDto {
    #[serde(rename = "key")]
    pub key: String,
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(rename = "proposalKind")]
    pub proposal_kind: String,
    #[serde(rename = "maxRisk")]
    pub max_risk: RiskLevelDto,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(WriteAllowanceDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        key: crate::required_field(&mut object, "key")?,
        chat_id: crate::required_field(&mut object, "chatId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        proposal_kind: crate::required_field(&mut object, "proposalKind")?,
        max_risk: crate::required_field(&mut object, "maxRisk")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
