// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WriteAllowanceListResponse {
    #[serde(rename = "allowances")]
    pub allowances: Vec<WriteAllowanceDto>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(WriteAllowanceListResponse, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        allowances: crate::required_field(&mut object, "allowances")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
