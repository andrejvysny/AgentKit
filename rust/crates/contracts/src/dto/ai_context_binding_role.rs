use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiContextBindingRole {
    #[serde(rename = "primary")]
    Primary,
    #[serde(rename = "reference")]
    Reference,
    #[serde(rename = "comparison")]
    Comparison,
}
crate::impl_json_via_serde!(AiContextBindingRole);
