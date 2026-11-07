// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentRevisionSourceDto {
    #[serde(rename = "create")]
    Create,
    #[serde(rename = "edit")]
    Edit,
    #[serde(rename = "model_change")]
    ModelChange,
    #[serde(rename = "restore")]
    Restore,
}
crate::impl_json_via_serde!(AgentRevisionSourceDto);
