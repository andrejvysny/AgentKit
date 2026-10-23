// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiAudioPartType {
    #[serde(rename = "audio")]
    Audio,
}
crate::impl_json_via_serde!(AiAudioPartType);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiAudioPartFormat {
    #[serde(rename = "wav")]
    Wav,
    #[serde(rename = "mp3")]
    Mp3,
}
crate::impl_json_via_serde!(AiAudioPartFormat);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiAudioPart {
    #[serde(rename = "type")]
    pub type_: AiAudioPartType,
    #[serde(rename = "data")]
    pub data: AiProviderKind,
    #[serde(rename = "format")]
    pub format: AiAudioPartFormat,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiAudioPart, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        data: crate::required_field(&mut object, "data")?,
        format: crate::required_field(&mut object, "format")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
