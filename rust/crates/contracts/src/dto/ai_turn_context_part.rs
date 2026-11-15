use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiTurnContextPartType {
    #[serde(rename = "turn_context")]
    TurnContext,
}
crate::impl_json_via_serde!(AiTurnContextPartType);

/// The turn context of a user message (roadmap 1.2): selected skills and
/// templates, source snapshots, the date and delta notes, built once when the
/// turn starts, persisted with the message and replayed unchanged. The text
/// is wrapped in `<turn_context>` so a model can tell it from what the user
/// typed. Transports send it as a plain text part.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiTurnContextPart {
    #[serde(rename = "type")]
    pub type_: AiTurnContextPartType,
    #[serde(rename = "text")]
    pub text: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiTurnContextPart, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        type_: crate::required_field(&mut object, "type")?,
        text: crate::required_field(&mut object, "text")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
