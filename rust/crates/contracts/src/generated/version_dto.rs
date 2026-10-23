// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VersionDto {
    #[serde(rename = "contractVersion")]
    pub contract_version: AiProviderKind,
    #[serde(rename = "restApiVersion")]
    pub rest_api_version: AiProviderKind,
    #[serde(
        rename = "packages",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub packages: Optional<BTreeMap<String, AiProviderKind>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(VersionDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        contract_version: crate::required_field(&mut object, "contractVersion")?,
        rest_api_version: crate::required_field(&mut object, "restApiVersion")?,
        packages: crate::optional_field(&mut object, "packages")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
