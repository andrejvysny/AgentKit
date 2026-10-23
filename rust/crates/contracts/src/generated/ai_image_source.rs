// Generated from packages/contracts/src via codegen/generate.mjs.
#![allow(unused_imports)]
use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiImageSourceUrlKind {
    #[serde(rename = "url")]
    Url,
}
crate::impl_json_via_serde!(AiImageSourceUrlKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiImageSourceUrl {
    #[serde(rename = "kind")]
    pub kind: AiImageSourceUrlKind,
    #[serde(rename = "url")]
    pub url: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiImageSourceUrl, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        url: crate::required_field(&mut object, "url")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiImageSourceDataKind {
    #[serde(rename = "data")]
    Data,
}
crate::impl_json_via_serde!(AiImageSourceDataKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiImageSourceData {
    #[serde(rename = "kind")]
    pub kind: AiImageSourceDataKind,
    #[serde(rename = "base64")]
    pub base64: AiProviderKind,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiImageSourceData, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        base64: crate::required_field(&mut object, "base64")?,
        media_type: crate::required_field(&mut object, "mediaType")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiImageSourceRefKind {
    #[serde(rename = "ref")]
    Ref,
}
crate::impl_json_via_serde!(AiImageSourceRefKind);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiImageSourceRef {
    #[serde(rename = "kind")]
    pub kind: AiImageSourceRefKind,
    #[serde(rename = "ref")]
    pub ref_: AiProviderKind,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiImageSourceRef, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        kind: crate::required_field(&mut object, "kind")?,
        ref_: crate::required_field(&mut object, "ref")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AiImageSource {
    Url(AiImageSourceUrl),
    Data(AiImageSourceData),
    Ref(AiImageSourceRef),
}
crate::impl_json_deserialize!(AiImageSource, |value: Value| {
    if let Ok(member) = <AiImageSourceUrl as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Url(member));
    }
    if let Ok(member) = <AiImageSourceData as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Data(member));
    }
    if let Ok(member) = <AiImageSourceRef as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Ref(member));
    }
    Err(crate::json_error("invalid AiImageSource"))
});
