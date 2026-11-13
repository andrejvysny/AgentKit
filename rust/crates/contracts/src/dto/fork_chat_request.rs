use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForkChatRequest {
    #[serde(rename = "fromMessageId")]
    pub from_message_id: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ForkChatRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        from_message_id: crate::required_field(&mut object, "fromMessageId")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
