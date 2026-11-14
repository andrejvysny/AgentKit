//! The durable task repository port. The engine drives claim/lease/settle
//! against this trait and never names a concrete store; an adapter crate
//! (the SQLite store) implements it.

use crate::{
    ClaimInput, ClaimedTask, EventRecord, Lease, LeaseFence, RecoveryMode, RetryTask, SettleTask,
    SubmitTask, TaskRecord,
};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskInputReason {
    UnknownDependency,
    EventSequence,
    InvalidForkPoint,
    ReplayIdentityMismatch,
}

/// How the engine tells repository faults apart without knowing the
/// adapter's error enum. One predicate per fault the engine reacts to.
pub trait RepositoryFault: std::error::Error + Send + Sync + 'static {
    /// Environmental fault around data that must land (database or
    /// filesystem); a bounded retry may succeed.
    fn is_transient(&self) -> bool;
    fn is_duplicate(&self) -> bool;
    fn is_lease_lost(&self) -> bool;
    fn is_invalid_transition(&self) -> bool;
    fn is_not_found(&self) -> bool;
    /// The message of an invalid-input refusal, when this fault is one.
    fn invalid_input(&self) -> Option<&str>;
    fn task_input_reason(&self) -> Option<TaskInputReason>;
    /// The scope whose session descriptor changed while staging a
    /// settlement, when this fault is that conflict.
    fn session_conflict(&self) -> Option<&str>;

    // --- payloads of the refusals a CONTEXT projects ----------------------
    //
    // Branching on `is_*` is enough to decide what to DO; a context that
    // also has to NAME the refusal (a failure code, a client message) needs
    // the refusal's own text, which is not recoverable from `Display` —
    // that renders the adapter's sentence, not the payload. One method per
    // refusal any context projects; `None` means "this fault is not that
    // refusal". The conformance suite pins every one of them, so a new
    // adapter cannot answer `None` and silently degrade a client's code.

    /// The record that was not found, when this fault is a `not found`.
    fn not_found(&self) -> Option<&str> {
        None
    }
    /// The rejected bytes of an idempotency replay: same identity,
    /// different content. Distinct from an answer conflict.
    fn input_conflict(&self) -> Option<&str> {
        None
    }
    /// The refusal of a second answer to a request already answered.
    fn answer_conflict(&self) -> Option<&str> {
        None
    }
    /// The busy scope whose mutation was refused.
    fn scope_busy(&self) -> Option<&str> {
        None
    }
    /// Why an answer can no longer be recorded.
    fn answer_stale(&self) -> Option<&str> {
        None
    }
    /// The task whose lease this fault says is gone.
    fn lease_lost_task(&self) -> Option<&str> {
        None
    }
    /// The refusal of an operator resolution for a receipt that is not
    /// uncertain, when this fault is that refusal.
    fn receipt_not_uncertain(&self) -> Option<&str> {
        None
    }
    /// The refusal of an operator resolution while the run may still record
    /// its own result, when this fault is that refusal.
    fn receipt_run_active(&self) -> Option<&str> {
        None
    }
    /// The managed-content refusal that asks the caller for the connector
    /// grants its selection needs, when this fault is that refusal.
    fn selected_connector_required(&self) -> bool {
        false
    }
    /// The message of a state-machine refusal, when this fault is one.
    fn invalid_transition(&self) -> Option<&str> {
        None
    }
    /// The message of a lost optimistic-concurrency version, when this fault
    /// is one: the write applied NOTHING and the caller re-reads.
    fn concurrent_modification(&self) -> Option<&str> {
        None
    }
    /// The class of an environmental fault: `database`, `filesystem` or
    /// `serialization`. `None` when the fault is not environmental.
    fn environmental_class(&self) -> Option<&'static str> {
        if self.is_transient() {
            Some("database")
        } else {
            None
        }
    }
    /// The adapter's own stable label for a fault the port has no predicate
    /// for — an adapter-internal failure whose name the context persists but
    /// cannot enumerate. `None` means "render the error's `Display`".
    fn label(&self) -> Option<&'static str> {
        None
    }
    /// An environmental fault around the substrate itself (database,
    /// filesystem or serialization): nothing the caller sent is wrong, and
    /// the failure is reported as an external one rather than classified.
    fn is_environmental(&self) -> bool {
        self.is_transient()
    }
}

/// A session-aware settlement payload: the work a settling turn promotes
/// inside its settle commit. Opaque to the engine except for the task
/// kinds it would admit and the ability to strip the promotion.
pub trait SessionSettlement: Clone + Send + Sync + 'static {
    /// Every task kind the settlement would stage.
    fn staged_kinds(&self) -> Vec<String>;
    /// The same settlement with its promotion removed: the descriptor
    /// fence is kept, nothing is converted or staged.
    fn without_promotion(self) -> Self;
}

#[async_trait]
pub trait TaskRepository: Clone + Send + Sync + 'static {
    type Error: RepositoryFault;
    type SessionSettle: SessionSettlement;

    async fn submit(&self, input: SubmitTask) -> Result<TaskRecord, Self::Error>;
    async fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, Self::Error>;
    async fn list_children(&self, task_id: &str) -> Result<Vec<TaskRecord>, Self::Error>;
    async fn interrupt_queued(&self, before: i64) -> Result<usize, Self::Error>;
    async fn interrupt_task(
        &self,
        task_id: &str,
        reason: &str,
        lease_token: Option<String>,
        now: i64,
    ) -> Result<bool, Self::Error>;
    async fn cancel_task(&self, task_id: &str, now: i64) -> Result<TaskRecord, Self::Error>;
    async fn resume_task(&self, task_id: &str, now: i64) -> Result<TaskRecord, Self::Error>;
    async fn recover(
        &self,
        now: i64,
        max_attempts: u32,
        retry_delay_ms: i64,
        mode: RecoveryMode,
    ) -> Result<Vec<TaskRecord>, Self::Error>;
    async fn claim_with_concurrency(
        &self,
        input: ClaimInput,
        concurrency: usize,
    ) -> Result<Option<ClaimedTask>, Self::Error>;
    async fn renew(&self, proof: LeaseFence, now: i64, ttl_ms: i64) -> Result<Lease, Self::Error>;
    async fn park_interrupted(&self, fence: LeaseFence, now: i64) -> Result<(), Self::Error>;
    async fn settle(&self, input: SettleTask) -> Result<TaskRecord, Self::Error>;
    async fn settle_session(
        &self,
        input: SettleTask,
        extra: Self::SessionSettle,
    ) -> Result<TaskRecord, Self::Error>;
    async fn retry_task(&self, input: RetryTask) -> Result<TaskRecord, Self::Error>;
    async fn submit_child(
        &self,
        proof: LeaseFence,
        input: SubmitTask,
        now: i64,
    ) -> Result<TaskRecord, Self::Error>;
    async fn update_progress(
        &self,
        proof: LeaseFence,
        progress: Value,
        now: i64,
    ) -> Result<(), Self::Error>;
    async fn next_seq(&self, task_id: &str) -> Result<u64, Self::Error>;
    async fn append_events(
        &self,
        proof: LeaseFence,
        events: Vec<Value>,
        now: i64,
    ) -> Result<Vec<EventRecord>, Self::Error>;
    async fn events_after_number(
        &self,
        task_id: &str,
        after_seq: Option<f64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, Self::Error>;
    async fn wait_for_children(&self, proof: LeaseFence, now: i64) -> Result<bool, Self::Error>;
    async fn check_dispatch(&self, proof: LeaseFence, now: i64) -> Result<(), Self::Error>;
}
