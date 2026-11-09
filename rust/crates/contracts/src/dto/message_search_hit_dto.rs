use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessageSearchHitDto {
    #[serde(rename = "chatId")]
    pub chat_id: String,
    #[serde(rename = "messageId")]
    pub message_id: String,
    #[serde(rename = "snippet")]
    pub snippet: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(MessageSearchHitDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        chat_id: crate::required_field(&mut object, "chatId")?,
        message_id: crate::required_field(&mut object, "messageId")?,
        snippet: crate::required_field(&mut object, "snippet")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
