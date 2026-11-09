use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AiToolCall {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "argumentsJson")]
    pub arguments_json: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(AiToolCall, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        id: crate::required_field(&mut object, "id")?,
        name: crate::required_field(&mut object, "name")?,
        arguments_json: crate::required_field(&mut object, "argumentsJson")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
