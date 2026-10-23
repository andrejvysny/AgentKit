use crate::CoreError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Trusted execution identity for a domain's atomic fencing check. The domain
/// must reject a superseded token in the same transaction as its mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchFence {
    pub task_id: String,
    pub attempt_id: String,
    pub fencing_token: u64,
}

/// A local preflight complements, but cannot replace, domain transaction fencing.
#[async_trait]
pub trait DispatchGuard: Send + Sync + std::fmt::Debug {
    async fn check(&self) -> Result<(), CoreError>;
    fn fence(&self) -> DispatchFence;
}
