use async_trait::async_trait;
use serde_json::Value;

use crate::GovernanceResult;

#[async_trait]
pub trait Authorization: Send + Sync {
    async fn authorize(
        &self,
        actor: &Value,
        action: &str,
        resource: &Value,
    ) -> GovernanceResult<()>;
}

#[derive(Clone, Debug)]
pub struct MutationIdentity {
    pub proposal_id: String,
    pub operation_id: String,
    pub fingerprint: String,
    pub execution_fence: Option<agentkit_core::DispatchFence>,
}

#[async_trait]
pub trait ProposalApplier: Send + Sync {
    /// When present, enforce execution_fence atomically with the mutation. A
    /// local preflight cannot prevent a worker becoming stale during this await.
    async fn apply(&self, identity: MutationIdentity, operations: Value)
    -> GovernanceResult<Value>;
    async fn reconcile(&self, identity: MutationIdentity) -> GovernanceResult<Option<Value>>;
}
