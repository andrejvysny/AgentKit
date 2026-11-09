use crate::{EngineResult, ExecutionContext};
use async_trait::async_trait;
use std::sync::Arc;

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
        uuid::Uuid::now_v7().to_string()
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
    async fn execute(&self, context: ExecutionContext) -> EngineResult<()>;
}

/// Best-effort observer of a durably settled task, run by the runner after
/// the settlement committed. Observers must be idempotent and must never
/// hold state only they know — every sweep they perform must be
/// reconstructible from durable records (that is what makes a dropped
/// observation merely late, never wrong).
#[async_trait]
pub trait TaskSettledObserver: Send + Sync {
    async fn observe_settled(&self, task: &agentkit_store::TaskRecord) -> EngineResult<()>;
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
    async fn verify(&self, context: ExecutionContext) -> EngineResult<VerificationReport>;
    async fn verify_pass(&self, input: VerificationInput) -> EngineResult<VerificationReport> {
        self.verify(input.context).await
    }
    async fn verify_optional(
        &self,
        input: VerificationInput,
    ) -> EngineResult<Option<VerificationReport>> {
        self.verify_pass(input).await.map(Some)
    }
}

pub type SharedRuntime = Arc<dyn Runtime>;
