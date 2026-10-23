// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AiContentPart {
    Text(AiTextPart),
    Image(AiImagePart),
}
crate::impl_json_deserialize!(AiContentPart, |value: Value| {
    if let Ok(member) = <AiTextPart as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Text(member));
    }
    if let Ok(member) = <AiImagePart as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Image(member));
    }
    Err(crate::json_error("invalid AiContentPart"))
});
