//! The SQLite store as the engine's task repository (the adapter side of
//! the `agentkit_engine::TaskRepository` port).

use crate::{
    ClaimInput, ClaimedTask, EventRecord, Lease, LeaseFence, ManagedInputReason,
    ReceiptTransitionReason, RecoveryMode, RetryTask, SettleTask, SqliteStore, StoreError,
    SubmitTask, TaskRecord,
};
use agentkit_engine::{EngineError, RepositoryFault, TaskRepository};
use async_trait::async_trait;
use serde_json::Value;

impl RepositoryFault for StoreError {
    fn is_transient(&self) -> bool {
        matches!(self, Self::Database(_) | Self::Io(_))
    }

    fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate(_) | Self::AnswerConflict(_))
    }

    fn is_lease_lost(&self) -> bool {
        matches!(self, Self::LeaseLost(_))
    }

    fn is_invalid_transition(&self) -> bool {
        matches!(
            self,
            Self::InvalidTransition(_)
                | Self::AgentInputTransition(_)
                | Self::AnswerStale(_)
                | Self::ReceiptTransition { .. }
        )
    }

    fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    fn invalid_input(&self) -> Option<&str> {
        match self {
            Self::InvalidInput(message)
            | Self::TaskInput { message, .. }
            | Self::ManagedInput { message, .. } => Some(message),
            _ => None,
        }
    }

    fn task_input_reason(&self) -> Option<agentkit_engine::TaskInputReason> {
        match self {
            Self::TaskInput { reason, .. } => Some(*reason),
            _ => None,
        }
    }

    fn session_conflict(&self) -> Option<&str> {
        match self {
            Self::SessionDescriptorChanged(scope) => Some(scope),
            _ => None,
        }
    }

    fn not_found(&self) -> Option<&str> {
        match self {
            Self::NotFound(message) => Some(message),
            _ => None,
        }
    }

    fn input_conflict(&self) -> Option<&str> {
        match self {
            Self::Duplicate(message) => Some(message),
            _ => None,
        }
    }

    fn answer_conflict(&self) -> Option<&str> {
        match self {
            Self::AnswerConflict(message) => Some(message),
            _ => None,
        }
    }

    fn scope_busy(&self) -> Option<&str> {
        match self {
            Self::ScopeBusy(scope) => Some(scope),
            _ => None,
        }
    }

    fn answer_stale(&self) -> Option<&str> {
        match self {
            Self::AnswerStale(message) => Some(message),
            _ => None,
        }
    }

    fn lease_lost_task(&self) -> Option<&str> {
        match self {
            Self::LeaseLost(task) => Some(task),
            _ => None,
        }
    }

    fn receipt_not_uncertain(&self) -> Option<&str> {
        match self {
            Self::ReceiptTransition {
                reason: ReceiptTransitionReason::NotUncertain,
                message,
            } => Some(message),
            _ => None,
        }
    }

    fn receipt_run_active(&self) -> Option<&str> {
        match self {
            Self::ReceiptTransition {
                reason: ReceiptTransitionReason::RunActive,
                message,
            } => Some(message),
            _ => None,
        }
    }

    fn selected_connector_required(&self) -> bool {
        matches!(
            self,
            Self::ManagedInput {
                reason: ManagedInputReason::SelectedConnectorRequired,
                ..
            }
        )
    }

    fn invalid_transition(&self) -> Option<&str> {
        match self {
            Self::InvalidTransition(message) | Self::AgentInputTransition(message) => Some(message),
            _ => None,
        }
    }

    fn concurrent_modification(&self) -> Option<&str> {
        match self {
            Self::Conflict(message) => Some(message),
            _ => None,
        }
    }

    fn environmental_class(&self) -> Option<&'static str> {
        match self {
            Self::Database(_) => Some("database"),
            Self::Io(_) => Some("filesystem"),
            Self::Json(_) => Some("serialization"),
            _ => None,
        }
    }

    fn label(&self) -> Option<&'static str> {
        match self {
            Self::InvalidImport { .. } => Some("invalid import"),
            Self::RecoveryUnreadable(_) => Some("recovery evidence unreadable"),
            Self::Internal(_) => Some("internal store error"),
            _ => None,
        }
    }

    fn is_environmental(&self) -> bool {
        matches!(self, Self::Database(_) | Self::Io(_) | Self::Json(_))
    }
}

impl From<StoreError> for EngineError<StoreError> {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[async_trait]
impl TaskRepository for SqliteStore {
    type Error = StoreError;
    /// The settlement record is the sessions context's; its
    /// `SessionSettlement` implementation lives there (ADR 0024 §2).
    type SessionSettle = crate::SessionSettle;

    async fn submit(&self, input: SubmitTask) -> Result<TaskRecord, StoreError> {
        SqliteStore::submit(self, input).await
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, StoreError> {
        SqliteStore::get_task(self, task_id).await
    }

    async fn list_children(&self, task_id: &str) -> Result<Vec<TaskRecord>, StoreError> {
        SqliteStore::list_children(self, task_id).await
    }

    async fn interrupt_queued(&self, before: i64) -> Result<usize, StoreError> {
        SqliteStore::interrupt_queued(self, before).await
    }

    async fn interrupt_task(
        &self,
        task_id: &str,
        reason: &str,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<bool, StoreError> {
        SqliteStore::interrupt_task(self, task_id, reason, lease_token, now).await
    }

    async fn cancel_task(&self, task_id: &str, now: i64) -> Result<TaskRecord, StoreError> {
        SqliteStore::cancel_task(self, task_id, now).await
    }

    async fn resume_task(&self, task_id: &str, now: i64) -> Result<TaskRecord, StoreError> {
        SqliteStore::resume_task(self, task_id, now).await
    }

    async fn recover(
        &self,
        now: i64,
        max_attempts: u32,
        retry_delay_ms: i64,
        mode: RecoveryMode,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        SqliteStore::recover(self, now, max_attempts, retry_delay_ms, mode).await
    }

    async fn claim_with_concurrency(
        &self,
        input: ClaimInput,
        concurrency: usize,
    ) -> Result<Option<ClaimedTask>, StoreError> {
        SqliteStore::claim_with_concurrency(self, input, concurrency).await
    }

    async fn renew(&self, proof: LeaseFence, now: i64, ttl_ms: i64) -> Result<Lease, StoreError> {
        SqliteStore::renew(self, proof, now, ttl_ms).await
    }

    async fn park_interrupted(&self, fence: LeaseFence, now: i64) -> Result<(), StoreError> {
        SqliteStore::park_interrupted(self, fence, now).await
    }

    async fn settle(&self, input: SettleTask) -> Result<TaskRecord, StoreError> {
        SqliteStore::settle(self, input).await
    }

    async fn settle_session(
        &self,
        input: SettleTask,
        extra: crate::SessionSettle,
    ) -> Result<TaskRecord, StoreError> {
        SqliteStore::settle_session(self, input, extra).await
    }

    async fn retry_task(&self, input: RetryTask) -> Result<TaskRecord, StoreError> {
        SqliteStore::retry_task(self, input).await
    }

    async fn submit_child(
        &self,
        proof: LeaseFence,
        input: SubmitTask,
        now: i64,
    ) -> Result<TaskRecord, StoreError> {
        SqliteStore::submit_child(self, proof, input, now).await
    }

    async fn update_progress(
        &self,
        proof: LeaseFence,
        progress: Value,
        now: i64,
    ) -> Result<(), StoreError> {
        SqliteStore::update_progress(self, proof, progress, now).await
    }

    async fn next_seq(&self, task_id: &str) -> Result<u64, StoreError> {
        SqliteStore::next_seq(self, task_id).await
    }

    async fn append_events(
        &self,
        proof: LeaseFence,
        events: Vec<Value>,
        now: i64,
    ) -> Result<Vec<EventRecord>, StoreError> {
        SqliteStore::append_events(self, proof, events, now).await
    }

    async fn events_after_number(
        &self,
        task_id: &str,
        after_seq: Option<f64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        SqliteStore::events_after_number(self, task_id, after_seq, limit).await
    }

    async fn wait_for_children(&self, proof: LeaseFence, now: i64) -> Result<bool, StoreError> {
        SqliteStore::wait_for_children(self, proof, now).await
    }

    async fn check_dispatch(&self, proof: LeaseFence, now: i64) -> Result<(), StoreError> {
        SqliteStore::check_dispatch(self, proof, now).await
    }
}
