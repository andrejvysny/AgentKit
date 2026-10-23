// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProposalDecisionDtoActor {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "policy")]
    Policy,
}
crate::impl_json_via_serde!(ProposalDecisionDtoActor);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProposalDecisionDto {
    #[serde(rename = "actor")]
    pub actor: ProposalDecisionDtoActor,
    #[serde(
        rename = "decidedBy",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub decided_by: Optional<AiProviderKind>,
    #[serde(
        rename = "policyId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub policy_id: Optional<AiProviderKind>,
    #[serde(
        rename = "reason",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reason: Optional<AiProviderKind>,
    #[serde(rename = "decidedAt")]
    pub decided_at: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ProposalDecisionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        actor: crate::required_field(&mut object, "actor")?,
        decided_by: crate::optional_field(&mut object, "decidedBy")?,
        policy_id: crate::optional_field(&mut object, "policyId")?,
        reason: crate::optional_field(&mut object, "reason")?,
        decided_at: crate::required_field(&mut object, "decidedAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
