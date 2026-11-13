use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentRevisionSourceDto {
    #[serde(rename = "create")]
    Create,
    #[serde(rename = "edit")]
    Edit,
    #[serde(rename = "model_change")]
    ModelChange,
    #[serde(rename = "restore")]
    Restore,
}
crate::impl_json_via_serde!(AgentRevisionSourceDto);
