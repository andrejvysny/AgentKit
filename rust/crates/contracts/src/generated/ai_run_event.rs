// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AiRunEvent {
    Started(AiRunStartedEvent),
    MessageDelta(AiRunMessageDeltaEvent),
    MessageReasoningDelta(AiRunMessageReasoningDeltaEvent),
    MessageCompleted(AiRunMessageCompletedEvent),
    ToolRequested(AiRunToolRequestedEvent),
    ToolRunning(AiRunToolRunningEvent),
    ToolSucceeded(AiRunToolSucceededEvent),
    ToolFailed(AiRunToolFailedEvent),
    Warning(AiRunWarningEvent),
    Usage(AiRunUsageEvent),
    ContextCompacted(AiRunContextCompactedEvent),
    ContextAdmitted(AiRunContextAdmittedEvent),
    Verification(AiRunVerificationEvent),
    InputApplied(AiRunInputAppliedEvent),
    Waiting(AiRunWaitingEvent),
    Completed(AiRunCompletedEvent),
    Failed(AiRunFailedEvent),
    Cancelled(AiRunCancelledEvent),
}
crate::impl_json_deserialize!(AiRunEvent, |value: Value| {
    if let Ok(member) = <AiRunStartedEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Started(member));
    }
    if let Ok(member) = <AiRunMessageDeltaEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::MessageDelta(member));
    }
    if let Ok(member) =
        <AiRunMessageReasoningDeltaEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::MessageReasoningDelta(member));
    }
    if let Ok(member) =
        <AiRunMessageCompletedEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::MessageCompleted(member));
    }
    if let Ok(member) =
        <AiRunToolRequestedEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::ToolRequested(member));
    }
    if let Ok(member) = <AiRunToolRunningEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::ToolRunning(member));
    }
    if let Ok(member) =
        <AiRunToolSucceededEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::ToolSucceeded(member));
    }
    if let Ok(member) = <AiRunToolFailedEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::ToolFailed(member));
    }
    if let Ok(member) = <AiRunWarningEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Warning(member));
    }
    if let Ok(member) = <AiRunUsageEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Usage(member));
    }
    if let Ok(member) =
        <AiRunContextCompactedEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::ContextCompacted(member));
    }
    if let Ok(member) =
        <AiRunContextAdmittedEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::ContextAdmitted(member));
    }
    if let Ok(member) = <AiRunVerificationEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::Verification(member));
    }
    if let Ok(member) = <AiRunInputAppliedEvent as crate::JsonDeserialize>::from_json(value.clone())
    {
        return Ok(Self::InputApplied(member));
    }
    if let Ok(member) = <AiRunWaitingEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Waiting(member));
    }
    if let Ok(member) = <AiRunCompletedEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Completed(member));
    }
    if let Ok(member) = <AiRunFailedEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Failed(member));
    }
    if let Ok(member) = <AiRunCancelledEvent as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Cancelled(member));
    }
    Err(crate::json_error("invalid AiRunEvent"))
});
