// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

crate::number_literal!(AiProviderContinuationVersion, 1);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiProviderContinuationScopeProtocol {
    #[serde(rename = "responses")]
    Responses,
}
crate::impl_json_via_serde!(AiProviderContinuationScopeProtocol);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiProviderContinuationScope {
    #[serde(rename = "providerId")]
    pub provider_id: String,
    #[serde(rename = "protocol")]
    pub protocol: AiProviderContinuationScopeProtocol,
    #[serde(rename = "connectionId")]
    pub connection_id: String,
    #[serde(rename = "generation", serialize_with = "crate::serialize_number")]
    pub generation: f64,
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "branchId")]
    pub branch_id: String,
    #[serde(rename = "model")]
    pub model: String,
}
crate::impl_json_deserialize!(AiProviderContinuationScope, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        provider_id: crate::required_field(&mut object, "providerId")?,
        protocol: crate::required_field(&mut object, "protocol")?,
        connection_id: crate::required_field(&mut object, "connectionId")?,
        generation: crate::required_field(&mut object, "generation")?,
        chat_id: crate::required_field(&mut object, "chatId")?,
        branch_id: crate::required_field(&mut object, "branchId")?,
        model: crate::required_field(&mut object, "model")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiProviderContinuationRunContent {
    #[serde(rename = "runId")]
    pub run_id: String,
    #[serde(rename = "digest")]
    pub digest: String,
}
crate::impl_json_deserialize!(AiProviderContinuationRunContent, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        run_id: crate::required_field(&mut object, "runId")?,
        digest: crate::required_field(&mut object, "digest")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AiProviderContinuation {
    #[serde(rename = "version")]
    pub version: AiProviderContinuationVersion,
    #[serde(rename = "scope")]
    pub scope: AiProviderContinuationScope,
    #[serde(
        rename = "runContent",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub run_content: Optional<AiProviderContinuationRunContent>,
    #[serde(rename = "messageCount", serialize_with = "crate::serialize_number")]
    pub message_count: f64,
    #[serde(rename = "messageDigest")]
    pub message_digest: String,
    #[serde(rename = "inputItems")]
    pub input_items: Vec<BTreeMap<String, Value>>,
}
crate::impl_json_deserialize!(AiProviderContinuation, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        version: crate::required_field(&mut object, "version")?,
        scope: crate::required_field(&mut object, "scope")?,
        run_content: crate::optional_field(&mut object, "runContent")?,
        message_count: crate::required_field(&mut object, "messageCount")?,
        message_digest: crate::required_field(&mut object, "messageDigest")?,
        input_items: crate::required_field(&mut object, "inputItems")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
