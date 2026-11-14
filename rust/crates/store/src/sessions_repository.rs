//! The SQLite store as the sessions context's session repository (the
//! adapter side of the `agentkit_sessions::SessionRepository` port).
//!
//! Every method forwards to the inherent method of the same name; the SQL
//! and the inherent API are unchanged. The queue payloads still have
//! store-local twins, so the three crossing methods map their fields.
use crate::{
    AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputStatus, AgentRecord, ChatRecord,
    ConversationTimestamp, CreateAgentInput, EventRecord, ExecutionReceiptRecord, MessageInput,
    MessageRecord, ResolveExecution, SqliteStore, StoreError, SubmitTask, TaskRecord,
};
use agentkit_sessions::{
    AnswerHumanRequest, AnswerOutcome, QueueAdmission, QueueOrStage, SessionMutation,
    SessionRefusal, StageQueuedTurn, apply_session_mutation,
};
use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
impl agentkit_sessions::SessionRepository for SqliteStore {
    type Error = StoreError;

    // -- the session aggregate ------------------------------------------------

    async fn create_chat(
        &self,
        chat_id: &str,
        title: Option<String>,
        metadata: Value,
        now: i64,
    ) -> Result<ChatRecord, StoreError> {
        SqliteStore::create_chat(self, chat_id, title, metadata, now).await
    }

    async fn get_chat(&self, chat_id: &str) -> Result<Option<ChatRecord>, StoreError> {
        SqliteStore::get_chat(self, chat_id).await
    }

    async fn patch_session(
        &self,
        chat_id: &str,
        mutation: SessionMutation,
        now: i64,
    ) -> Result<ChatRecord, StoreError> {
        self.mutate_chat(chat_id, now, move |chat| {
            apply_session_mutation(chat, &mutation, now).map_err(|refusal| match refusal {
                SessionRefusal::NotFound(message) => StoreError::NotFound(message),
                SessionRefusal::InvalidInput(message) => StoreError::InvalidInput(message),
            })?;
            chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
            Ok(chat.clone())
        })
        .await
    }

    async fn messages(
        &self,
        chat_id: &str,
        include_inactive: bool,
    ) -> Result<Vec<MessageRecord>, StoreError> {
        SqliteStore::messages(self, chat_id, include_inactive).await
    }

    async fn fork_chat(
        &self,
        chat_id: &str,
        from_message_id: &str,
        new_chat_id: &str,
        now: i64,
    ) -> Result<(ChatRecord, Vec<MessageRecord>), StoreError> {
        SqliteStore::fork_chat(self, chat_id, from_message_id, new_chat_id, now).await
    }

    async fn list_context_summaries(&self, session: &str) -> Result<Vec<Value>, StoreError> {
        SqliteStore::list_context_summaries(self, session).await
    }

    async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>, StoreError> {
        SqliteStore::get_setting(self, namespace, key).await
    }

    async fn events(
        &self,
        task_id: &str,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        SqliteStore::events(self, task_id, after_seq, limit).await
    }

    async fn list_tasks(&self, scope_id: Option<String>) -> Result<Vec<TaskRecord>, StoreError> {
        SqliteStore::list_tasks(self, scope_id).await
    }

    // -- the steering input queue ---------------------------------------------

    async fn create_input(
        &self,
        input: CreateAgentInput,
        now: i64,
    ) -> Result<(AgentInputRecord, bool), StoreError> {
        SqliteStore::create_input(self, input, now).await
    }

    async fn get_input(&self, input_id: String) -> Result<Option<AgentInputRecord>, StoreError> {
        SqliteStore::get_input(self, input_id).await
    }

    async fn list_inputs(
        &self,
        query: AgentInputQuery,
    ) -> Result<Vec<AgentInputRecord>, StoreError> {
        SqliteStore::list_inputs(self, query).await
    }

    async fn transition_input(
        &self,
        input_id: String,
        from: Vec<AgentInputStatus>,
        to: AgentInputStatus,
        patch: AgentInputPatch,
        now: i64,
    ) -> Result<AgentInputRecord, StoreError> {
        SqliteStore::transition_input(self, input_id, from, to, patch, now).await
    }

    async fn queue_or_stage_turn(&self, input: QueueOrStage) -> Result<QueueAdmission, StoreError> {
        SqliteStore::queue_or_stage_turn(self, input).await
    }

    async fn stage_queued_turn(
        &self,
        input: StageQueuedTurn,
    ) -> Result<Option<String>, StoreError> {
        SqliteStore::stage_queued_turn(self, input).await
    }

    async fn answer_human_request(
        &self,
        input: AnswerHumanRequest,
    ) -> Result<AnswerOutcome, StoreError> {
        SqliteStore::answer_human_request(self, input).await
    }

    async fn request_interrupt(&self, task_id: String, now: i64) -> Result<(), StoreError> {
        SqliteStore::request_interrupt(self, task_id, now).await
    }

    async fn request_compact(
        &self,
        session_id: String,
        task_kind: String,
        expected_descriptor: Value,
        now: i64,
    ) -> Result<(), StoreError> {
        SqliteStore::request_compact(self, session_id, task_kind, expected_descriptor, now).await
    }

    async fn revoke_standing_approval(
        &self,
        session_id: String,
        grant_id: String,
        now: i64,
    ) -> Result<Value, StoreError> {
        SqliteStore::revoke_standing_approval(self, session_id, grant_id, now).await
    }

    async fn submit_turn(
        &self,
        input: SubmitTask,
        messages: Vec<MessageInput>,
    ) -> Result<TaskRecord, StoreError> {
        SqliteStore::submit_turn(self, input, messages).await
    }

    // -- profiles, catalogs and managed content --------------------------------

    async fn get_agent(&self, agent_id: String) -> Result<Option<AgentRecord>, StoreError> {
        SqliteStore::get_agent(self, agent_id).await
    }

    async fn get_provider(&self, id: &str) -> Result<Option<Value>, StoreError> {
        SqliteStore::get_provider(self, id).await
    }

    async fn get_provider_capabilities(&self, id: &str) -> Result<Option<Value>, StoreError> {
        SqliteStore::get_provider_capabilities(self, id).await
    }

    async fn list_provider_models(&self, id: &str) -> Result<Vec<Value>, StoreError> {
        SqliteStore::list_provider_models(self, id).await
    }

    async fn get_skill(&self, id: String) -> Result<Option<Value>, StoreError> {
        SqliteStore::get_skill(self, id).await
    }

    async fn resolve_run_content(
        &self,
        selection: Value,
        authorized_connectors: Vec<String>,
        tolerate_unavailable: bool,
    ) -> Result<Value, StoreError> {
        SqliteStore::resolve_run_content(
            self,
            selection,
            authorized_connectors,
            tolerate_unavailable,
        )
        .await
    }

    async fn run_profile_dispatch_refusal(
        &self,
        task_id: String,
    ) -> Result<Option<String>, StoreError> {
        SqliteStore::run_profile_dispatch_refusal(self, task_id).await
    }

    // -- execution receipts (ADR 0020) -----------------------------------------

    async fn list_executions_by_chat(
        &self,
        chat_id: String,
    ) -> Result<Vec<ExecutionReceiptRecord>, StoreError> {
        SqliteStore::list_executions_by_chat(self, chat_id).await
    }

    async fn resolve_execution(
        &self,
        execution_id: String,
        request: ResolveExecution,
    ) -> Result<ExecutionReceiptRecord, StoreError> {
        SqliteStore::resolve_execution(self, execution_id, request).await
    }
}
