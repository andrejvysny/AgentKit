// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalGrantRefDto {
    #[serde(rename = "sessionId")]
    pub session_id: AiProviderKind,
    #[serde(rename = "grantId")]
    pub grant_id: AiProviderKind,
    #[serde(rename = "digest")]
    pub digest: AiProviderKind,
}
crate::impl_json_deserialize!(ApprovalGrantRefDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        session_id: crate::required_field(&mut object, "sessionId")?,
        grant_id: crate::required_field(&mut object, "grantId")?,
        digest: crate::required_field(&mut object, "digest")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
