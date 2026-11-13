use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentLifecycleDto {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "archived")]
    Archived,
}
crate::impl_json_via_serde!(AgentLifecycleDto);
