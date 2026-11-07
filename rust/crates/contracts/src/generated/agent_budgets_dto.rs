// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentBudgetsDto {
    #[serde(
        rename = "maxToolIterations",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_tool_iterations: Optional<f64>,
    #[serde(
        rename = "maxToolCallsPerIteration",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_tool_calls_per_iteration: Optional<f64>,
    #[serde(
        rename = "maxOutputTokens",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub max_output_tokens: Optional<f64>,
    #[serde(
        rename = "attemptBudgetMs",
        default,
        skip_serializing_if = "Optional::is_absent",
        serialize_with = "crate::serialize_optional_number"
    )]
    pub attempt_budget_ms: Optional<f64>,
}
crate::impl_json_deserialize!(AgentBudgetsDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        max_tool_iterations: crate::optional_field(&mut object, "maxToolIterations")?,
        max_tool_calls_per_iteration: crate::optional_field(
            &mut object,
            "maxToolCallsPerIteration",
        )?,
        max_output_tokens: crate::optional_field(&mut object, "maxOutputTokens")?,
        attempt_budget_ms: crate::optional_field(&mut object, "attemptBudgetMs")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
