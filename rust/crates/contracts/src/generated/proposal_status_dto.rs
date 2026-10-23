// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProposalStatusDto {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "applying")]
    Applying,
    #[serde(rename = "applied")]
    Applied,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "invalidated")]
    Invalidated,
}
crate::impl_json_via_serde!(ProposalStatusDto);
