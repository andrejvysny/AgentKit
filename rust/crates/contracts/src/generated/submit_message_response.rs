// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SubmitMessageResponse {
    #[serde(rename = "chatId")]
    pub chat_id: AiProviderKind,
    #[serde(rename = "runId")]
    pub run_id: AiProviderKind,
    #[serde(rename = "userMessageId")]
    pub user_message_id: AiProviderKind,
    #[serde(rename = "assistantMessageId")]
    pub assistant_message_id: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(SubmitMessageResponse, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        chat_id: crate::required_field(&mut object, "chatId")?,
        run_id: crate::required_field(&mut object, "runId")?,
        user_message_id: crate::required_field(&mut object, "userMessageId")?,
        assistant_message_id: crate::required_field(&mut object, "assistantMessageId")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
