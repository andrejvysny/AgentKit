use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ProposalStatusDto {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "applying")]
    Applying,
    #[serde(rename = "applied")]
    Applied,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "invalidated")]
    Invalidated,
}
crate::impl_json_via_serde!(ProposalStatusDto);
