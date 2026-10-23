// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessageSearchResponse {
    #[serde(rename = "hits")]
    pub hits: Vec<MessageSearchHitDto>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(MessageSearchResponse, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        hits: crate::required_field(&mut object, "hits")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
