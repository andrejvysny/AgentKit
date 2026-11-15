use crate::{
    AiModelBodyExtras, AiModelDialect, AiModelFamily, AiModelLimits, AiModelLoadMode,
    AiModelMessageRules, AiModelReasoning, AiModelRecovery, AiModelSampling,
    AiModelToolChoicePolicy,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The per-model tool-use profile (native tool use T2.1, ADR 0030). Resolved
/// when a session starts and frozen with it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiModelToolProfile {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "family")]
    pub family: AiModelFamily,
    #[serde(rename = "dialect")]
    pub dialect: AiModelDialect,
    /// Tools visible without loading.
    #[serde(rename = "directTools")]
    pub direct_tools: Vec<String>,
    /// Upper bound for the `tools` array.
    #[serde(rename = "maxVisibleTools", serialize_with = "crate::serialize_number")]
    pub max_visible_tools: f64,
    #[serde(rename = "loadMode")]
    pub load_mode: AiModelLoadMode,
    #[serde(rename = "toolChoice")]
    pub tool_choice: AiModelToolChoicePolicy,
    #[serde(rename = "sampling")]
    pub sampling: AiModelSampling,
    #[serde(rename = "reasoning")]
    pub reasoning: AiModelReasoning,
    #[serde(rename = "bodyExtras")]
    pub body_extras: AiModelBodyExtras,
    #[serde(rename = "messageRules")]
    pub message_rules: AiModelMessageRules,
    #[serde(rename = "limits")]
    pub limits: AiModelLimits,
    #[serde(rename = "recovery")]
    pub recovery: AiModelRecovery,
    /// Toolsets the conformance matrix qualified for this model.
    #[serde(rename = "qualifiedToolsets")]
    pub qualified_toolsets: Vec<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiModelToolProfile, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        family: crate::required_field(&mut object, "family")?,
        dialect: crate::required_field(&mut object, "dialect")?,
        direct_tools: crate::required_field(&mut object, "directTools")?,
        max_visible_tools: crate::required_field(&mut object, "maxVisibleTools")?,
        load_mode: crate::required_field(&mut object, "loadMode")?,
        tool_choice: crate::required_field(&mut object, "toolChoice")?,
        sampling: crate::required_field(&mut object, "sampling")?,
        reasoning: crate::required_field(&mut object, "reasoning")?,
        body_extras: crate::required_field(&mut object, "bodyExtras")?,
        message_rules: crate::required_field(&mut object, "messageRules")?,
        limits: crate::required_field(&mut object, "limits")?,
        recovery: crate::required_field(&mut object, "recovery")?,
        qualified_toolsets: crate::required_field(&mut object, "qualifiedToolsets")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
