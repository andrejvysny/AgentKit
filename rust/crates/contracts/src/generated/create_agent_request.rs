// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAgentRequest {
    #[serde(rename = "definition")]
    pub definition: AgentDefinitionDto,
}
crate::impl_json_deserialize!(CreateAgentRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        definition: crate::required_field(&mut object, "definition")?,
    };
    crate::reject_extra_fields(&object)?;
    Ok(output)
});
