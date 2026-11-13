use crate::{MessageDto, Optional};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MessagePageDto {
    #[serde(rename = "items")]
    pub items: Vec<MessageDto>,
    #[serde(
        rename = "nextCursor",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub next_cursor: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(MessagePageDto, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        items: crate::required_field(&mut object, "items")?,
        next_cursor: crate::optional_field(&mut object, "nextCursor")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
