// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProposalDto {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "chatId")]
    pub chat_id: AiProviderKind,
    #[serde(rename = "runId", default, skip_serializing_if = "Optional::is_absent")]
    pub run_id: Optional<AiProviderKind>,
    #[serde(rename = "scopeKey")]
    pub scope_key: AiProviderKind,
    #[serde(
        rename = "actionId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub action_id: Optional<AiProviderKind>,
    #[serde(rename = "toolName")]
    pub tool_name: AiProviderKind,
    #[serde(rename = "kind")]
    pub kind: AiProviderKind,
    #[serde(rename = "risk")]
    pub risk: RiskLevelDto,
    #[serde(rename = "status")]
    pub status: ProposalStatusDto,
    #[serde(
        rename = "summary",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub summary: Optional<AiProviderKind>,
    #[serde(rename = "warnings")]
    pub warnings: Vec<AiProviderKind>,
    #[serde(rename = "truncated")]
    pub truncated: bool,
    #[serde(
        rename = "decision",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub decision: Optional<ProposalDecisionDto>,
    #[serde(
        rename = "outcome",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub outcome: Optional<ApplyOutcomeDto>,
    #[serde(rename = "createdAt")]
    pub created_at: AiProviderKind,
    #[serde(
        rename = "decidedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub decided_at: Optional<AiProviderKind>,
    #[serde(
        rename = "claimedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub claimed_at: Optional<AiProviderKind>,
    #[serde(
        rename = "appliedAt",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub applied_at: Optional<AiProviderKind>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ProposalDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        chat_id: crate::required_field(&mut object, "chatId")?,
        run_id: crate::optional_field(&mut object, "runId")?,
        scope_key: crate::required_field(&mut object, "scopeKey")?,
        action_id: crate::optional_field(&mut object, "actionId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        kind: crate::required_field(&mut object, "kind")?,
        risk: crate::required_field(&mut object, "risk")?,
        status: crate::required_field(&mut object, "status")?,
        summary: crate::optional_field(&mut object, "summary")?,
        warnings: crate::required_field(&mut object, "warnings")?,
        truncated: crate::required_field(&mut object, "truncated")?,
        decision: crate::optional_field(&mut object, "decision")?,
        outcome: crate::optional_field(&mut object, "outcome")?,
        created_at: crate::required_field(&mut object, "createdAt")?,
        decided_at: crate::optional_field(&mut object, "decidedAt")?,
        claimed_at: crate::optional_field(&mut object, "claimedAt")?,
        applied_at: crate::optional_field(&mut object, "appliedAt")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
