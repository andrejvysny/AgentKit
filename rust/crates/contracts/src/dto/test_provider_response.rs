use crate::Optional;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TestProviderResponse {
    #[serde(rename = "ok")]
    pub ok: bool,
    #[serde(rename = "error", default, skip_serializing_if = "Optional::is_absent")]
    pub error: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(TestProviderResponse, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        ok: crate::required_field(&mut object, "ok")?,
        error: crate::optional_field(&mut object, "error")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
