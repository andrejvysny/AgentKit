use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::HostResult;

/// Subagent coordination tools that change durable child state. They keep
/// `effect: "read"` so the permission modes still admit them, but must carry
/// execution receipts: on an attempt replay the recorded result is served
/// instead of re-running the spawn/message/cancel/resume.
pub fn is_mutating_subagent_tool(name: &str) -> bool {
    matches!(
        name,
        "subagent_spawn" | "subagent_message" | "subagent_cancel" | "subagent_resume"
    )
}

/// Per-turn delegation guidance (subagent instructions) consulted by the
/// executor for session turns. Implementations read durable/host config,
/// never process-memory caches a restart could not rebuild.
#[async_trait]
pub trait DelegationGuidance: Send + Sync {
    async fn guidance(&self, chat_id: &str) -> Option<String>;
}

#[async_trait]
pub trait ChatProviderFactory: Send + Sync {
    fn available(&self) -> bool {
        true
    }
    async fn snapshot(
        &self,
        _provider_id: &str,
        _model: &str,
        _cancel: CancellationToken,
    ) -> HostResult<Option<Value>> {
        Ok(None)
    }
    async fn resolve(
        &self,
        snapshot: &Value,
        cancel: CancellationToken,
    ) -> HostResult<Arc<dyn agentkit_core::ChatProvider>>;
}

#[async_trait]
pub trait ContextProvider: Send + Sync {
    async fn resolve(&self, input: Value, cancel: CancellationToken) -> HostResult<Value>;
}

#[async_trait]
pub trait AttachmentResolver: Send + Sync {
    async fn resolve(
        &self,
        reference: &str,
        context: &AttachmentResolveContext,
        cancel: CancellationToken,
    ) -> HostResult<Option<ResolvedAttachment>>;
}

pub struct AttachmentResolveContext {
    pub chat_id: String,
}

pub struct ResolvedAttachment {
    pub media_type: String,
    pub base64: String,
}

#[async_trait]
pub trait Authorization: Send + Sync {
    async fn authorize(&self, actor: &Value, action: &str, resource: &Value) -> HostResult<()>;
}

#[async_trait]
pub trait UsageAuthorizer: Send + Sync {
    async fn authorize(&self, task_id: &str, usage: &Value) -> HostResult<()>;
    async fn record(&self, _usage: &Value) -> HostResult<()> {
        Ok(())
    }
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
    async fn apply(&self, identity: MutationIdentity, operations: Value) -> HostResult<Value>;
    async fn reconcile(&self, identity: MutationIdentity) -> HostResult<Option<Value>>;
}
