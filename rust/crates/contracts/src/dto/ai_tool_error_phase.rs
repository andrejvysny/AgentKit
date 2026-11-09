use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiToolErrorPhase {
    #[serde(rename = "validation")]
    Validation,
    #[serde(rename = "guard")]
    Guard,
    #[serde(rename = "execution")]
    Execution,
}
crate::impl_json_via_serde!(AiToolErrorPhase);
