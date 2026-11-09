use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentTaskStatusDto {
    #[serde(rename = "queued")]
    Queued,
    #[serde(rename = "running")]
    Running,
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "waiting_approval")]
    WaitingApproval,
    #[serde(rename = "waiting_child")]
    WaitingChild,
    #[serde(rename = "uncertain")]
    Uncertain,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
}
crate::impl_json_via_serde!(AgentTaskStatusDto);
