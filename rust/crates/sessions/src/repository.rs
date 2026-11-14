//! The sessions context's durable repository port (ADR 0024 §1-§3).
//!
//! The session service drives create/fork, the steering input queue, the
//! run-profile catalogs it compiles a turn from, and the execution receipts
//! it resolves, all against this trait. It never names a concrete store: the
//! SQLite adapter (`agentkit-store`) implements it, `agentkit-testkit` holds
//! the in-memory twin, and both run the same conformance suite.
//!
//! One trait rather than `SessionRepo`/`InputQueue`/`ProfileRepo`: the
//! aggregates share their transactions — `queue_or_stage_turn` reads the chat
//! to decide between queueing and staging, `patch_session` validates the
//! descriptor it writes — and every consumer of the port needs the whole
//! surface, so a split would only multiply bounds without narrowing anything.
//!
//! Methods that must be atomic across aggregates stay single methods (as
//! engine's `settle_session`): `queue_or_stage_turn`, `stage_queued_turn`,
//! `answer_human_request`, `patch_session` and `submit_turn` each commit
//! cross-aggregate work in one adapter job, and a caller cannot observe or
//! reproduce their intermediate states.
//!
//! Faults are the adapter's own type, reached only through
//! [`RepositoryFault`] predicates (ADR 0024 §3): the service never matches a
//! concrete error enum, so an adapter is free to organise its failures as it
//! likes as long as the conformance suite's predicate answers hold.

use crate::{
    AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputStatus, AgentRecord, ChatRecord,
    CreateAgentInput, ExecutionReceiptRecord, QueueAdmission, QueueOrStage, ResolveExecution,
    StageQueuedTurn,
};
use agentkit_engine::{EventRecord, MessageInput, SubmitTask, TaskRecord};
use async_trait::async_trait;
use serde_json::Value;

pub use agentkit_engine::RepositoryFault;

/// What a session-service repository is: the engine's task port and this
/// context's session port, both reporting ONE fault type. A composition root
/// names one concrete store; the context only ever sees these two ports and
/// the fault they share, so `SessionsError<R::Error>` is the single error
/// every session call reports.
pub trait SessionBackend:
    SessionRepository + agentkit_engine::TaskRepository<Error = <Self as SessionRepository>::Error>
{
}

impl<R> SessionBackend for R where
    R: SessionRepository + agentkit_engine::TaskRepository<Error = <R as SessionRepository>::Error>
{
}

/// The ONE fault a [`SessionBackend`] reports, spelled short: both ports of a
/// backend share it, so a session call's error is always
/// `SessionsError<SessionFault<R>>`.
pub type SessionFault<R> = <R as SessionRepository>::Error;

/// The durable store the sessions context drives. See the module docs.
#[async_trait]
pub trait SessionRepository: Clone + Send + Sync + 'static {
    /// The adapter's fault type, branchable only through [`RepositoryFault`].
    type Error: RepositoryFault;

    // -- the session aggregate -------------------------------------------------

    /// Create the chat row that carries a session's history and metadata.
    async fn create_chat(
        &self,
        chat_id: &str,
        title: Option<String>,
        metadata: Value,
        now: i64,
    ) -> Result<ChatRecord, Self::Error>;

    /// The chat row, or `None` when the id names nothing.
    async fn get_chat(&self, chat_id: &str) -> Result<Option<ChatRecord>, Self::Error>;

    /// Apply a session-metadata write in one job: the record is re-read, the
    /// mutation validated ([`crate::apply_session_mutation`]) and stored, so
    /// a concurrent turn cannot slip between the check and the write.
    async fn patch_session(
        &self,
        chat_id: &str,
        mutation: crate::SessionMutation,
        now: i64,
    ) -> Result<ChatRecord, Self::Error>;

    /// The chat's messages, newest-history order as stored. `include_inactive`
    /// keeps the messages a compaction retired.
    async fn messages(
        &self,
        chat_id: &str,
        include_inactive: bool,
    ) -> Result<Vec<crate::MessageRecord>, Self::Error>;

    /// Copy a chat and its messages up to `from_message_id` into
    /// `new_chat_id`, atomically. Pending inputs and the provider
    /// continuation do not travel.
    async fn fork_chat(
        &self,
        chat_id: &str,
        from_message_id: &str,
        new_chat_id: &str,
        now: i64,
    ) -> Result<(ChatRecord, Vec<crate::MessageRecord>), Self::Error>;

    /// The session's persisted context summaries, newest first.
    async fn list_context_summaries(&self, session: &str) -> Result<Vec<Value>, Self::Error>;

    /// One settings row, or `None` when the namespace/key was never written.
    async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>, Self::Error>;

    /// A task's events after `after_seq`, capped at `limit`.
    async fn events(
        &self,
        task_id: &str,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, Self::Error>;

    /// Every task in `scope_id`'s scope, or every task when it is `None`.
    async fn list_tasks(&self, scope_id: Option<String>) -> Result<Vec<TaskRecord>, Self::Error>;

    // -- the steering input queue ------------------------------------------------

    /// Store one steering input. The `bool` says whether it is new or an
    /// idempotent replay of the caller's `inputId`.
    async fn create_input(
        &self,
        input: CreateAgentInput,
        now: i64,
    ) -> Result<(AgentInputRecord, bool), Self::Error>;

    /// One input by id, or `None`.
    async fn get_input(&self, input_id: String) -> Result<Option<AgentInputRecord>, Self::Error>;

    /// The inputs matching `query`.
    async fn list_inputs(
        &self,
        query: AgentInputQuery,
    ) -> Result<Vec<AgentInputRecord>, Self::Error>;

    /// Move one input's status, refusing unless it is currently in `from`.
    async fn transition_input(
        &self,
        input_id: String,
        from: Vec<AgentInputStatus>,
        to: AgentInputStatus,
        patch: AgentInputPatch,
        now: i64,
    ) -> Result<AgentInputRecord, Self::Error>;

    /// The unified admission (ADR 0017 part 1): store the input and, when the
    /// session's scope is free, stage it as the next turn — in one job, so a
    /// second admission cannot slip between the busy check and the write.
    async fn queue_or_stage_turn(&self, input: QueueOrStage)
    -> Result<QueueAdmission, Self::Error>;

    /// Stage one already-queued input when the scope is free; `None` when it
    /// is busy, so the input waits for the next flush.
    async fn stage_queued_turn(
        &self,
        input: StageQueuedTurn,
    ) -> Result<Option<String>, Self::Error>;

    /// Record a human's answer to a pending request and, when the answer
    /// settles the turn it blocked, do that in the same job.
    async fn answer_human_request(
        &self,
        input: crate::AnswerHumanRequest,
    ) -> Result<crate::AnswerOutcome, Self::Error>;

    /// Ask the running task to interrupt itself; the executor observes it.
    async fn request_interrupt(&self, task_id: String, now: i64) -> Result<(), Self::Error>;

    /// Ask for deferred compaction at the next turn of `task_kind`, refusing
    /// when `expected_descriptor` is no longer the session's descriptor.
    async fn request_compact(
        &self,
        session_id: String,
        task_kind: String,
        expected_descriptor: Value,
        now: i64,
    ) -> Result<(), Self::Error>;

    /// Stop one standing approval from matching. Idempotent; running calls are
    /// not interrupted.
    async fn revoke_standing_approval(
        &self,
        session_id: String,
        grant_id: String,
        now: i64,
    ) -> Result<Value, Self::Error>;

    /// Submit a session turn with its frozen messages: the task and its
    /// messages land together, and the turn's input is marked staged in the
    /// same commit.
    async fn submit_turn(
        &self,
        input: SubmitTask,
        messages: Vec<MessageInput>,
    ) -> Result<TaskRecord, Self::Error>;

    // -- profiles, catalogs and managed content ----------------------------------

    /// The pinned agent definition, or `None` when the id was never published.
    async fn get_agent(&self, agent_id: String) -> Result<Option<AgentRecord>, Self::Error>;

    /// A provider row, or `None`.
    async fn get_provider(&self, id: &str) -> Result<Option<Value>, Self::Error>;

    /// A provider's capability row, or `None`.
    async fn get_provider_capabilities(&self, id: &str) -> Result<Option<Value>, Self::Error>;

    /// The catalog models of a provider, in catalog order.
    async fn list_provider_models(&self, id: &str) -> Result<Vec<Value>, Self::Error>;

    /// A skill definition, or `None`.
    async fn get_skill(&self, id: String) -> Result<Option<Value>, Self::Error>;

    /// Resolve a managed-content selection against the catalog.
    async fn resolve_run_content(
        &self,
        selection: Value,
        authorized_connectors: Vec<String>,
        tolerate_unavailable: bool,
    ) -> Result<Value, Self::Error>;

    /// The reason a frozen run profile refuses dispatch, or `None` when the
    /// turn may run.
    async fn run_profile_dispatch_refusal(
        &self,
        task_id: String,
    ) -> Result<Option<String>, Self::Error>;

    // -- execution receipts (ADR 0020) ---------------------------------------------

    /// Every receipt recorded against a chat, newest first.
    async fn list_executions_by_chat(
        &self,
        chat_id: String,
    ) -> Result<Vec<ExecutionReceiptRecord>, Self::Error>;

    /// An operator resolution of an uncertain receipt.
    async fn resolve_execution(
        &self,
        execution_id: String,
        request: ResolveExecution,
    ) -> Result<ExecutionReceiptRecord, Self::Error>;
}
