use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AiToolEffect {
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "write")]
    Write,
}
crate::impl_json_via_serde!(AiToolEffect);
