use crate::{EngineResult, ExecutionContext, TaskRecord, TaskRepository};
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
pub trait TaskExecutor<R: TaskRepository>: Send + Sync {
    fn handles_verification(&self) -> bool {
        false
    }
    fn resolves_provider_snapshots(&self) -> bool {
        false
    }
    /// Target-aware view of [`Self::handles_verification`]. Wrappers that
    /// route by the frozen execution target MUST override this (and the
    /// sibling below) so the wrapped routes keep the capabilities they
    /// declared: a dispatch wrapper must never silently reclassify a
    /// turn's verification or provider-snapshot behaviour.
    /// Unwrapped executors inherit their flag unchanged.
    fn handles_verification_for(&self, _payload: &serde_json::Value) -> bool {
        self.handles_verification()
    }
    /// Target-aware view of [`Self::resolves_provider_snapshots`].
    fn resolves_provider_snapshots_for(&self, _payload: &serde_json::Value) -> bool {
        self.resolves_provider_snapshots()
    }
    async fn execute(&self, context: ExecutionContext<R>) -> EngineResult<(), R::Error>;
}

/// Failure of a best-effort settled observation. Observers never see the
/// repository, so their error is not tied to its error type.
pub type ObserverError = Box<dyn std::error::Error + Send + Sync>;

/// Best-effort observer of a durably settled task, run by the runner after
/// the settlement committed. Observers must be idempotent and must never
/// hold state only they know — every sweep they perform must be
/// reconstructible from durable records (that is what makes a dropped
/// observation merely late, never wrong).
#[async_trait]
pub trait TaskSettledObserver: Send + Sync {
    async fn observe_settled(&self, task: &TaskRecord) -> Result<(), ObserverError>;
}

#[derive(Clone, Debug)]
pub struct VerificationReport {
    pub status: String,
    pub deficiencies: Vec<String>,
}

pub struct VerificationInput<R: TaskRepository> {
    pub context: ExecutionContext<R>,
    pub final_content: String,
    pub tool_call_count: usize,
    pub pass: u32,
}

#[async_trait]
pub trait VerificationHook<R: TaskRepository>: Send + Sync {
    async fn verify(
        &self,
        context: ExecutionContext<R>,
    ) -> EngineResult<VerificationReport, R::Error>;
    async fn verify_pass(
        &self,
        input: VerificationInput<R>,
    ) -> EngineResult<VerificationReport, R::Error> {
        self.verify(input.context).await
    }
    async fn verify_optional(
        &self,
        input: VerificationInput<R>,
    ) -> EngineResult<Option<VerificationReport>, R::Error> {
        self.verify_pass(input).await.map(Some)
    }
}

pub type SharedRuntime = Arc<dyn Runtime>;
