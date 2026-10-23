// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForkChatRequest {
    #[serde(rename = "fromMessageId")]
    pub from_message_id: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ForkChatRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        from_message_id: crate::required_field(&mut object, "fromMessageId")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
