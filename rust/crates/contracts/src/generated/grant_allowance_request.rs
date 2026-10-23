// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GrantAllowanceRequest {
    #[serde(rename = "toolName")]
    pub tool_name: AiProviderKind,
    #[serde(rename = "proposalKind")]
    pub proposal_kind: AiProviderKind,
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
