// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiRunEventType {
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "run.message.delta")]
    RunMessageDelta,
    #[serde(rename = "run.message.completed")]
    RunMessageCompleted,
    #[serde(rename = "run.tool.requested")]
    RunToolRequested,
    #[serde(rename = "run.tool.running")]
    RunToolRunning,
    #[serde(rename = "run.tool.succeeded")]
    RunToolSucceeded,
    #[serde(rename = "run.tool.failed")]
    RunToolFailed,
    #[serde(rename = "run.warning")]
    RunWarning,
    #[serde(rename = "run.usage")]
    RunUsage,
    #[serde(rename = "run.verification")]
    RunVerification,
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "run.cancelled")]
    RunCancelled,
}
crate::impl_json_via_serde!(AiRunEventType);
