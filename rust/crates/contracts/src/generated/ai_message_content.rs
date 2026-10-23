// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AiMessageContent {
    Text(AiProviderKind),
    Parts(Vec<AiContentPart>),
}
crate::impl_json_deserialize!(AiMessageContent, |value: Value| {
    if let Ok(member) = <AiProviderKind as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Text(member));
    }
    if let Ok(member) = <Vec<AiContentPart> as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Parts(member));
    }
    Err(crate::json_error("invalid AiMessageContent"))
});
