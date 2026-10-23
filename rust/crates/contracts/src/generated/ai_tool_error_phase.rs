// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiToolErrorPhase {
    #[serde(rename = "validation")]
    Validation,
    #[serde(rename = "guard")]
    Guard,
    #[serde(rename = "execution")]
    Execution,
}
crate::impl_json_via_serde!(AiToolErrorPhase);
