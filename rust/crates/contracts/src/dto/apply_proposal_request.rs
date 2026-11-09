use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ApplyProposalRequest {
    #[serde(rename = "operationId")]
    pub operation_id: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ApplyProposalRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        operation_id: crate::required_field(&mut object, "operationId")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
