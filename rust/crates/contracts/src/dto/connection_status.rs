use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConnectionStatus {
    #[serde(rename = "disconnected")]
    Disconnected,
    #[serde(rename = "connecting")]
    Connecting,
    #[serde(rename = "connected")]
    Connected,
    #[serde(rename = "needs_authentication")]
    NeedsAuthentication,
    #[serde(rename = "failed")]
    Failed,
}
crate::impl_json_via_serde!(ConnectionStatus);
