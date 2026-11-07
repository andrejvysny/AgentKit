// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentLifecycleDto {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "archived")]
    Archived,
}
crate::impl_json_via_serde!(AgentLifecycleDto);
