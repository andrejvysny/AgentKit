// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSkillSelectionDto {
    #[serde(rename = "id")]
    pub id: AiProviderKind,
    #[serde(rename = "revision")]
    pub revision: AiProviderKind,
}
crate::impl_json_deserialize!(AgentSkillSelectionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        revision: crate::required_field(&mut object, "revision")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
