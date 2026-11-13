use crate::Optional;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProposalDecisionRequest {
    #[serde(
        rename = "reason",
        default,
        skip_serializing_if = "Optional::is_absent"
    )]
    pub reason: Optional<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}
crate::impl_json_deserialize!(ProposalDecisionRequest, |value: Value| {
    let mut object = crate::json_object(value)?;
    let output = Self {
        reason: crate::optional_field(&mut object, "reason")?,
        extra: object.into_iter().collect(),
    };
    Ok(output)
});
