use crate::{AiAudioPart, AiImagePart, AiTextPart, AiTurnContextPart};
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum AiContentPart {
    Text(AiTextPart),
    Image(AiImagePart),
    Audio(AiAudioPart),
    TurnContext(AiTurnContextPart),
}
crate::impl_json_deserialize!(AiContentPart, |value: Value| {
    if let Ok(member) = <AiTextPart as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Text(member));
    }
    if let Ok(member) = <AiImagePart as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Image(member));
    }
    if let Ok(member) = <AiAudioPart as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::Audio(member));
    }
    if let Ok(member) = <AiTurnContextPart as crate::JsonDeserialize>::from_json(value.clone()) {
        return Ok(Self::TurnContext(member));
    }
    Err(crate::json_error("invalid AiContentPart"))
});
