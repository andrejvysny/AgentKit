// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiContextBindingRole {
    #[serde(rename = "primary")]
    Primary,
    #[serde(rename = "reference")]
    Reference,
    #[serde(rename = "comparison")]
    Comparison,
}
crate::impl_json_via_serde!(AiContextBindingRole);
