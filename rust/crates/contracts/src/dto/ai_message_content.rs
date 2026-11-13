use crate::AiContentPart;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AiMessageContent {
    Text(String),
    Parts(Vec<AiContentPart>),
}
crate::impl_json_deserialize!(AiMessageContent, |value: Value| {
    if let Ok(member) = <String as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Text(member));
    }
    if let Ok(member) = <Vec<AiContentPart> as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Parts(member));
    }
    Err(crate::json_error("invalid AiMessageContent"))
});
