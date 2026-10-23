use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{ExecutionContext, HostResult};

pub trait Runtime: Send + Sync {
    fn now_ms(&self) -> i64;
    fn id(&self) -> String;
    fn random(&self) -> f64 {
        (uuid::Uuid::new_v4().as_u128() >> 75) as f64 / ((1u64 << 53) - 1) as f64
    }
}

pub struct SystemRuntime;

impl Runtime for SystemRuntime {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

#[async_trait]
pub trait TaskExecutor: Send + Sync {
    fn handles_verification(&self) -> bool {
        false
    }
    fn resolves_provider_snapshots(&self) -> bool {
        false
    }
    async fn execute(&self, context: ExecutionContext) -> HostResult<()>;
}

#[async_trait]
pub trait ChatProviderFactory: Send + Sync {
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
pub struct VerificationReport {
    pub status: String,
    pub deficiencies: Vec<String>,
}

pub struct VerificationInput {
    pub context: ExecutionContext,
    pub final_content: String,
    pub tool_call_count: usize,
    pub pass: u32,
}

#[async_trait]
pub trait VerificationHook: Send + Sync {
    async fn verify(&self, context: ExecutionContext) -> HostResult<VerificationReport>;
    async fn verify_pass(&self, input: VerificationInput) -> HostResult<VerificationReport> {
        self.verify(input.context).await
    }
    async fn verify_optional(
        &self,
        input: VerificationInput,
    ) -> HostResult<Option<VerificationReport>> {
        self.verify_pass(input).await.map(Some)
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

pub type SharedRuntime = Arc<dyn Runtime>;
