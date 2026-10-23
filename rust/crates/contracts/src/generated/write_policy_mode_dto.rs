// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum WritePolicyModeDto {
    #[serde(rename = "auto_readonly_confirm_writes")]
    AutoReadonlyConfirmWrites,
    #[serde(rename = "confirm_all_writes")]
    ConfirmAllWrites,
    #[serde(rename = "auto_all")]
    AutoAll,
}
crate::impl_json_via_serde!(WritePolicyModeDto);
