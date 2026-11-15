use crate::Optional;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunToolRecordEventType {
    #[serde(rename = "run.tool.record")]
    RunToolRecord,
}
crate::impl_json_via_serde!(AiRunToolRecordEventType);

/// Outcome class of one tool call (TU-35); shared with the failure type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiToolOutcomeClass {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "ok_after_repair")]
    OkAfterRepair,
    #[serde(rename = "unknown_tool")]
    UnknownTool,
    #[serde(rename = "invalid_json")]
    InvalidJson,
    #[serde(rename = "invalid_structure")]
    InvalidStructure,
    #[serde(rename = "invalid_value")]
    InvalidValue,
    #[serde(rename = "precondition")]
    Precondition,
    #[serde(rename = "guard_refused")]
    GuardRefused,
    #[serde(rename = "approval_denied")]
    ApprovalDenied,
    #[serde(rename = "tool_error")]
    ToolError,
    #[serde(rename = "timeout")]
    Timeout,
    #[serde(rename = "repeat_identical")]
    RepeatIdentical,
    #[serde(rename = "announced_no_call")]
    AnnouncedNoCall,
}
crate::impl_json_via_serde!(AiToolOutcomeClass);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunToolRecordEventData {
    #[serde(rename = "toolCallId")]
    pub tool_call_id: String,
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(
        rename = "canonicalTool",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub canonical_tool: Optional<String>,
    #[serde(rename = "step", serialize_with = "crate::serialize_number")]
    pub step: f64,
    #[serde(rename = "model")]
    pub model: String,
    #[serde(
        rename = "provider",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub provider: Optional<String>,
    #[serde(rename = "lane", default, skip_serializing_if = "Optional::is_absent")]
    pub lane: Optional<String>,
    #[serde(
        rename = "dialect",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub dialect: Optional<String>,
    #[serde(
        rename = "dialectVersion",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub dialect_version: Optional<String>,
    #[serde(
        rename = "profile",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub profile: Optional<String>,
    #[serde(
        rename = "toolset",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub toolset: Optional<String>,
    #[serde(rename = "outcomeClass")]
    pub outcome_class: AiToolOutcomeClass,
    #[serde(
        rename = "failureCode",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub failure_code: Optional<String>,
    #[serde(rename = "aliasesUsed")]
    pub aliases_used: Vec<String>,
    #[serde(rename = "valuesConverted", serialize_with = "crate::serialize_number")]
    pub values_converted: f64,
    #[serde(rename = "salvaged")]
    pub salvaged: bool,
    #[serde(rename = "repairAttempt", serialize_with = "crate::serialize_number")]
    pub repair_attempt: f64,
    #[serde(rename = "durationMs", serialize_with = "crate::serialize_number")]
    pub duration_ms: f64,
    #[serde(rename = "argumentBytes", serialize_with = "crate::serialize_number")]
    pub argument_bytes: f64,
    #[serde(rename = "resultBytes", serialize_with = "crate::serialize_number")]
    pub result_bytes: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunToolRecordEventData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        tool_call_id: crate::required_field(&mut object, "toolCallId")?,
        tool_name: crate::required_field(&mut object, "toolName")?,
        canonical_tool: crate::optional_field(&mut object, "canonicalTool")?,
        step: crate::required_field(&mut object, "step")?,
        model: crate::required_field(&mut object, "model")?,
        provider: crate::optional_field(&mut object, "provider")?,
        lane: crate::optional_field(&mut object, "lane")?,
        dialect: crate::optional_field(&mut object, "dialect")?,
        dialect_version: crate::optional_field(&mut object, "dialectVersion")?,
        profile: crate::optional_field(&mut object, "profile")?,
        toolset: crate::optional_field(&mut object, "toolset")?,
        outcome_class: crate::required_field(&mut object, "outcomeClass")?,
        failure_code: crate::optional_field(&mut object, "failureCode")?,
        aliases_used: crate::required_field(&mut object, "aliasesUsed")?,
        values_converted: crate::required_field(&mut object, "valuesConverted")?,
        salvaged: crate::required_field(&mut object, "salvaged")?,
        repair_attempt: crate::required_field(&mut object, "repairAttempt")?,
        duration_ms: crate::required_field(&mut object, "durationMs")?,
        argument_bytes: crate::required_field(&mut object, "argumentBytes")?,
        result_bytes: crate::required_field(&mut object, "resultBytes")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiRunToolRecordEvent {
    #[serde(rename = "type")]
    pub type_: AiRunToolRecordEventType,
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "timestamp")]
    pub timestamp: String,
    #[serde(rename = "contractVersion")]
    pub contract_version: String,
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "seq", serialize_with = "crate::serialize_number")]
    pub seq: f64,
    #[serde(
        rename = "attemptId",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub attempt_id: Optional<String>,
    #[serde(rename = "data")]
    pub data: AiRunToolRecordEventData,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiRunToolRecordEvent, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        run_id: crate::required_field(&mut object, "runId")?,
        timestamp: crate::required_field(&mut object, "timestamp")?,
        contract_version: crate::required_field(&mut object, "contractVersion")?,
        event_id: crate::required_field(&mut object, "eventId")?,
        seq: crate::required_field(&mut object, "seq")?,
        attempt_id: crate::optional_field(&mut object, "attemptId")?,
        data: crate::required_field(&mut object, "data")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
