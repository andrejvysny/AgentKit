use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiContextBindingStatus {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "missing")]
    Missing,
    #[serde(rename = "stale")]
    Stale,
}
crate::impl_json_via_serde!(AiContextBindingStatus);
