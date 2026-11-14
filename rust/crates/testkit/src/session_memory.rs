//! In-memory `SessionRepository` mirroring the SQLite store's observable
//! behaviour (the reference); every mutating call is atomic, and the
//! cross-aggregate methods (`queue_or_stage_turn`, `stage_queued_turn`,
//! `answer_human_request`, `patch_session`, `submit_turn`, `fork_chat`) hold
//! the one lock across their whole check-and-write, exactly as the store's
//! actor job does.

use agentkit_engine::{
    EventRecord, MessageInput, RepositoryFault, SubmitTask, TaskInputReason, TaskRecord, TaskStatus,
};
use agentkit_kernel::ConversationTimestamp;
use agentkit_sessions::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputSettlePolicy,
    AgentInputStatus, AgentRecord, AnswerHumanRequest, AnswerOutcome, AnswerReceipt, ChatRecord,
    CreateAgentInput, ExecutionReceiptRecord, ExecutionReceiptStatus, ExecutionResolution,
    MessageRecord, PROVIDER_MODELS_NAMESPACE, QueueAdmission, QueueOrStage, RECEIPT_NOT_UNCERTAIN,
    RECEIPT_RUN_ACTIVE, ResolutionOutcome, ResolveExecution, SESSION_METADATA_KEY, SessionMutation,
    SessionRefusal, StageQueuedTurn, StagedCandidate, apply_session_mutation,
    execution_state_for_grant, find_active_standing_grant, is_approval_request,
    parse_answer_expiry, parse_iso_millis, plan_request_of, read_execution_state,
    read_standing_key, run_profile_digest, with_execution, with_standing, writable_standing_grants,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::Mutex;

/// The provider-catalog namespaces, mirroring `agentkit-store`'s: the
/// settings rows a turn's pinned profile is checked against.
pub const PROVIDERS_NAMESPACE: &str = "agentkit.compat.providers";
/// `provider_catalog::PROVIDER_CAPABILITIES_NAMESPACE`.
pub const PROVIDER_CAPABILITIES_NAMESPACE: &str = "agentkit.compat.provider-capabilities";
/// `context_summaries::CONTEXT_SUMMARIES_NAMESPACE`.
pub const CONTEXT_SUMMARIES_NAMESPACE: &str = "agentkit.compat.context-summaries";
/// The namespace the in-memory catalog keeps published prompt templates in
/// (`managed_content`'s own `prompt_templates` table has no namespace; this
/// one stands in so `resolve_run_content` has a lookup to make).
pub const PROMPT_TEMPLATES_NAMESPACE: &str = "agentkit.compat.prompt-templates";

/// Faults of [`InMemorySessionRepository`]; variants mirror the store's, so
/// every predicate [`RepositoryFault`] answers means the same thing here as
/// it does for `StoreError`.
#[derive(Debug, thiserror::Error)]
pub enum InMemorySessionRepositoryError {
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("duplicate record: {0}")]
    Duplicate(String),
    #[error("duplicate record: {0}")]
    AnswerConflict(String),
    #[error("concurrent modification: {0}")]
    Conflict(String),
    #[error("scope busy: {0}")]
    ScopeBusy(String),
    #[error("session descriptor changed while staging settlement: {0}")]
    SessionDescriptorChanged(String),
    #[error("lease lost for task {0}")]
    LeaseLost(String),
    #[error("invalid transition: {0}")]
    InvalidTransition(String),
    #[error("invalid transition: {0}")]
    AnswerStale(String),
    #[error("invalid transition: {0}")]
    AgentInputTransition(String),
    #[error("invalid transition: {message}")]
    ReceiptTransition {
        reason: ReceiptRefusal,
        message: String,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid input: {message}")]
    TaskInput {
        reason: TaskInputReason,
        message: String,
    },
    #[error("invalid input: {0}")]
    SelectedConnectorRequired(String),
    #[error("recovery evidence unreadable: {0}")]
    RecoveryUnreadable(String),
    #[error("database error: {0}")]
    Environmental(&'static str),
}

/// Which receipt-resolution refusal a [`InMemorySessionRepositoryError::ReceiptTransition`]
/// carries; mirrors the store's `ReceiptTransitionReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptRefusal {
    NotUncertain,
    RunActive,
}

impl RepositoryFault for InMemorySessionRepositoryError {
    fn is_transient(&self) -> bool {
        matches!(self, Self::Environmental(_))
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
            | Self::SelectedConnectorRequired(message) => Some(message),
            _ => None,
        }
    }

    fn task_input_reason(&self) -> Option<TaskInputReason> {
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
                reason: ReceiptRefusal::NotUncertain,
                message,
            } => Some(message),
            _ => None,
        }
    }

    fn receipt_run_active(&self) -> Option<&str> {
        match self {
            Self::ReceiptTransition {
                reason: ReceiptRefusal::RunActive,
                message,
            } => Some(message),
            _ => None,
        }
    }

    fn selected_connector_required(&self) -> bool {
        matches!(self, Self::SelectedConnectorRequired(_))
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
            Self::Environmental(class) => Some(class),
            _ => None,
        }
    }

    fn label(&self) -> Option<&'static str> {
        match self {
            Self::RecoveryUnreadable(_) => Some("recovery evidence unreadable"),
            _ => None,
        }
    }

    fn is_environmental(&self) -> bool {
        matches!(self, Self::Environmental(_))
    }
}

impl From<agentkit_kernel::TimestampOutOfRange> for InMemorySessionRepositoryError {
    fn from(error: agentkit_kernel::TimestampOutOfRange) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

use InMemorySessionRepositoryError as Fault;

pub type Result<T, E = InMemorySessionRepositoryError> = std::result::Result<T, E>;

/// One row of the settings table: the store's generic key/value home, which
/// the catalog, the summaries and the provider models all live in.
#[derive(Clone)]
struct Setting {
    namespace: String,
    key: String,
    value: Value,
}

/// One published skill document, as the managed-content table keeps it.
#[derive(Clone)]
struct Skill {
    record: Value,
}

#[derive(Clone, Default)]
struct State {
    chats: Vec<ChatRecord>,
    messages: Vec<MessageRecord>,
    inputs: Vec<AgentInputRecord>,
    tasks: Vec<TaskRecord>,
    events: Vec<EventRecord>,
    agents: Vec<AgentRecord>,
    skills: Vec<Skill>,
    receipts: Vec<ExecutionReceiptRecord>,
    settings: Vec<Setting>,
    input_order: i64,
    message_seq: u64,
}

/// A `SessionRepository` held in memory. Clones share one state.
#[derive(Clone, Default)]
pub struct InMemorySessionRepository {
    state: Arc<Mutex<State>>,
}

impl InMemorySessionRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// The whole check-and-write runs under one lock; a returned error leaves
    /// the state exactly as it was, like a rolled-back transaction.
    async fn atomic<T>(&self, operation: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut guard = self.state.lock().await;
        let mut next = guard.clone();
        let out = operation(&mut next)?;
        *guard = next;
        Ok(out)
    }
}

impl Default for Skill {
    fn default() -> Self {
        Self {
            record: Value::Object(serde_json::Map::new()),
        }
    }
}

fn live(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Queued
            | TaskStatus::Running
            | TaskStatus::Interrupted
            | TaskStatus::WaitingApproval
    )
}

fn text(now: i64) -> Result<String> {
    Ok(ConversationTimestamp::from_epoch_millis(now)?
        .wire_text()?
        .into_owned())
}

// ---------------------------------------------------------------------------
// State helpers: the store's read paths, one per table.
// ---------------------------------------------------------------------------

impl State {
    fn chat(&self, id: &str) -> Result<ChatRecord> {
        self.chats
            .iter()
            .find(|chat| chat.id == id)
            .cloned()
            .ok_or_else(|| Fault::NotFound(id.into()))
    }

    fn input(&self, id: &str) -> Option<AgentInputRecord> {
        self.inputs
            .iter()
            .find(|record| record.input_id == id)
            .cloned()
    }

    fn required_input(&self, id: &str) -> Result<AgentInputRecord> {
        self.input(id)
            .ok_or_else(|| Fault::NotFound(format!("agent input {id} does not exist")))
    }

    fn task(&self, id: &str) -> Option<TaskRecord> {
        self.tasks.iter().find(|task| task.task_id == id).cloned()
    }

    fn required_task(&self, id: &str) -> Result<TaskRecord> {
        self.task(id).ok_or_else(|| Fault::NotFound(id.into()))
    }

    fn save_task(&mut self, task: &TaskRecord) {
        match self
            .tasks
            .iter_mut()
            .find(|slot| slot.task_id == task.task_id)
        {
            Some(slot) => *slot = task.clone(),
            None => self.tasks.push(task.clone()),
        }
    }

    fn save_chat(&mut self, chat: &ChatRecord) {
        match self.chats.iter_mut().find(|slot| slot.id == chat.id) {
            Some(slot) => *slot = chat.clone(),
            None => self.chats.push(chat.clone()),
        }
    }

    fn setting(&self, namespace: &str, key: &str) -> Option<Value> {
        self.settings
            .iter()
            .find(|row| row.namespace == namespace && row.key == key)
            .map(|row| row.value.clone())
    }

    fn namespace(&self, namespace: &str) -> Vec<(String, Value)> {
        self.settings
            .iter()
            .filter(|row| row.namespace == namespace)
            .map(|row| (row.key.clone(), row.value.clone()))
            .collect()
    }

    /// The store's `messages(chat_id)` listing: insertion order.
    fn chat_messages(&self, chat_id: &str) -> Vec<MessageRecord> {
        self.messages
            .iter()
            .filter(|message| message.chat_id == chat_id)
            .cloned()
            .collect()
    }

    /// `session_ops::last_message_of_run`: the newest message the run wrote.
    fn last_message_of_run(&self, chat_id: &str, run_id: &str) -> Option<MessageRecord> {
        self.messages
            .iter()
            .filter(|message| {
                message.chat_id == chat_id && message.run_id.as_deref() == Some(run_id)
            })
            .max_by_key(|message| message.order_key)
            .cloned()
    }

    fn skill(&self, id: &str) -> Option<Value> {
        // `split_skill_id`: an id without a `:`-separated source and path
        // names no skill, exactly as the SQL lookup finds no row.
        let (source, path) = id.split_once(':')?;
        if source.is_empty() || path.is_empty() {
            return None;
        }
        self.skills
            .iter()
            .find(|skill| {
                skill.record["sourceId"].as_str() == Some(source)
                    && skill.record["path"].as_str() == Some(path)
            })
            .map(|skill| skill.record.clone())
    }
}

// ---------------------------------------------------------------------------
// Message append — `conversations::append`, kept behaviour-for-behaviour.
// ---------------------------------------------------------------------------

fn next_message_id(state: &mut State) -> String {
    state.message_seq += 1;
    format!("message-{}", state.message_seq)
}

/// `agentkit_engine::MessageInput` -> durable row through the store's own
/// projection rules (role gate, explicit-parent gate, active-leaf parent
/// choice, order/depth/branch accounting, activation).
fn append_message(state: &mut State, input: MessageInput, now: i64) -> Result<MessageRecord> {
    if !["user", "assistant", "tool", "system"].contains(&input.role.as_str()) {
        return Err(Fault::InvalidInput("invalid message role".into()));
    }
    let mut chat = state.chat(&input.chat_id)?;
    if !input.activate && input.parent_message_id.is_none() {
        return Err(Fault::InvalidInput(
            "chain append requires explicit parent".into(),
        ));
    }
    let messages = state.chat_messages(&chat.id);
    let parent_id = if input.explicit_root {
        None
    } else {
        input.parent_message_id.clone().or_else(|| {
            messages
                .iter()
                .filter(|message| message.active)
                .max_by_key(|message| message.depth)
                .map(|message| message.id.clone())
        })
    };
    let parent = parent_id
        .as_ref()
        .map(|id| {
            messages
                .iter()
                .find(|message| message.id == *id)
                .filter(|message| !message.archived)
                .ok_or_else(|| Fault::NotFound(id.clone()))
        })
        .transpose()?;
    let active = input.activate
        || parent.is_some_and(|parent| parent.active)
            && !messages
                .iter()
                .any(|message| message.parent_message_id == parent_id && message.active);
    let message = MessageRecord {
        id: input.id.unwrap_or_else(|| next_message_id(state)),
        chat_id: chat.id.clone(),
        run_id: input.run_id,
        role: input.role,
        content: input.content,
        parent_message_id: parent_id.clone(),
        tool_call_id: input.tool_call_id,
        tool_calls: input.tool_calls,
        model_result_json: input.model_result_json,
        metadata: input.metadata,
        order_key: messages
            .iter()
            .map(|message| message.order_key)
            .max()
            .unwrap_or(0)
            + 1,
        depth: parent.map_or(0, |parent| parent.depth + 1),
        branch_index: messages
            .iter()
            .filter(|message| message.parent_message_id == parent_id)
            .map(|message| message.branch_index + 1)
            .max()
            .unwrap_or(0),
        active,
        archived: false,
        created_at: ConversationTimestamp::from_epoch_millis(now)?,
    };
    if state.messages.iter().any(|slot| slot.id == message.id) {
        return Err(Fault::Duplicate(message.id));
    }
    state.messages.push(message.clone());
    if input.activate {
        activate(state, &message.id)?;
    }
    chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
    state.save_chat(&chat);
    Ok(message)
}

/// `conversations::activate`: the activated message's active-child chain,
/// walked to its leaf, and that leaf's ancestry become the one active path;
/// every other message OF THE SAME CHAT's active flag drops.
fn activate(state: &mut State, message_id: &str) -> Result<()> {
    let target = state
        .messages
        .iter()
        .find(|message| message.id == message_id)
        .cloned()
        .ok_or_else(|| Fault::NotFound(message_id.into()))?;
    if target.archived {
        return Err(Fault::NotFound(message_id.into()));
    }
    let messages = state.chat_messages(&target.chat_id);
    let mut leaf = target.clone();
    loop {
        let kids: Vec<&MessageRecord> = messages
            .iter()
            .filter(|message| message.parent_message_id.as_deref() == Some(&leaf.id))
            .filter(|message| !message.archived)
            .collect();
        let Some(child) = kids
            .iter()
            .find(|message| message.active)
            .or_else(|| kids.iter().min_by_key(|message| message.branch_index))
            .copied()
            .cloned()
        else {
            break;
        };
        leaf = child;
    }
    let mut path: Vec<String> = vec![];
    let mut cursor = Some(leaf.id);
    while let Some(id) = cursor {
        let node = messages
            .iter()
            .find(|message| message.id == id)
            .ok_or_else(|| Fault::NotFound(id.clone()))?;
        path.push(id.clone());
        cursor = node.parent_message_id.clone();
    }
    for message in &mut state.messages {
        if message.chat_id == target.chat_id {
            message.active = path.contains(&message.id);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent-input queue helpers — `agent_inputs.rs`, kept behaviour-for-behaviour.
// ---------------------------------------------------------------------------

/// Allowed targets from `pending`, per kind; every non-pending status is
/// terminal, so this is the whole machine (mirrors
/// `agentkit_store::AGENT_INPUT_TRANSITIONS`).
const INPUT_TRANSITIONS: [(AgentInputKind, &[AgentInputStatus]); 3] = [
    (
        AgentInputKind::Steer,
        &[
            AgentInputStatus::Applied,
            AgentInputStatus::Converted,
            AgentInputStatus::Cancelled,
        ],
    ),
    (
        AgentInputKind::FollowUp,
        &[AgentInputStatus::Converted, AgentInputStatus::Cancelled],
    ),
    (
        AgentInputKind::HumanRequest,
        &[AgentInputStatus::Answered, AgentInputStatus::Cancelled],
    ),
];

fn assert_transition(
    record: &AgentInputRecord,
    from: &[AgentInputStatus],
    to: AgentInputStatus,
) -> Result<()> {
    if !from.contains(&record.status) {
        return Err(Fault::AgentInputTransition(format!(
            "Input {} is {:?}, expected one of {from:?}",
            record.input_id, record.status
        )));
    }
    let legal = record.status == AgentInputStatus::Pending
        && INPUT_TRANSITIONS
            .iter()
            .find(|(kind, _)| *kind == record.kind)
            .is_some_and(|(_, targets)| targets.contains(&to));
    if !legal {
        return Err(Fault::AgentInputTransition(format!(
            "Input {} ({:?}) cannot move {:?} -> {to:?}",
            record.input_id, record.kind, record.status
        )));
    }
    Ok(())
}

/// `validate_create_agent_input`.
fn validate_create(input: &CreateAgentInput) -> Result<()> {
    let bad = |message: &str| Err(Fault::InvalidInput(message.to_owned()));
    for (field, value) in [
        ("inputId", Some(input.input_id.as_str())),
        ("sessionId", Some(input.session_id.as_str())),
        ("contentHash", Some(input.content_hash.as_str())),
    ] {
        if value.is_none_or(str::is_empty) {
            return bad(&format!("{field} must be a non-empty string."));
        }
    }
    if input.on_settled.is_some() && input.kind != AgentInputKind::Steer {
        return bad("onSettled is only valid for steer.");
    }
    if input.kind == AgentInputKind::HumanRequest {
        for (field, value) in [
            ("attemptId", input.attempt_id.as_deref()),
            ("toolCallId", input.tool_call_id.as_deref()),
        ] {
            if value.is_none_or(str::is_empty) {
                return bad(&format!("{field} is required for human_request."));
            }
        }
    } else if input.attempt_id.is_some()
        || input.tool_call_id.is_some()
        || input.expires_at.is_some()
    {
        return bad("attemptId, toolCallId and expiresAt are only valid for human_request.");
    }
    if input.target_task_id.as_deref().is_some_and(str::is_empty) {
        return bad("targetTaskId must be non-empty when present.");
    }
    if input
        .expires_at
        .as_deref()
        .is_some_and(|value| parse_iso_millis(value).is_none())
    {
        return bad("expiresAt must be a valid ISO timestamp.");
    }
    Ok(())
}

/// `insert_input`: idempotent on `inputId`, and the store's `order` counter.
fn insert_input(
    state: &mut State,
    input: CreateAgentInput,
    now_text: &str,
) -> Result<(AgentInputRecord, bool)> {
    validate_create(&input)?;
    if let Some(existing) = state.input(&input.input_id) {
        if existing.session_id != input.session_id
            || existing.kind != input.kind
            || existing.content_hash != input.content_hash
            // A mailbox row (`agent_id` set) can never satisfy a session replay.
            || existing.agent_id.is_some()
        {
            return Err(Fault::Duplicate(format!(
                "input_conflict: input {} already exists with different kind, session, or content.",
                input.input_id
            )));
        }
        return Ok((existing, false));
    }
    state.input_order += 1;
    let record = AgentInputRecord {
        input_id: input.input_id,
        session_id: input.session_id,
        agent_id: None,
        kind: input.kind,
        status: AgentInputStatus::Pending,
        content: input.content,
        content_hash: input.content_hash,
        order: state.input_order,
        target_task_id: input.target_task_id,
        on_settled: input.on_settled,
        attempt_id: input.attempt_id,
        tool_call_id: input.tool_call_id,
        expires_at: input.expires_at,
        applied_task_id: None,
        applied_message_id: None,
        answer_id: None,
        created_at: now_text.into(),
        updated_at: now_text.into(),
    };
    state.inputs.push(record.clone());
    Ok((record, true))
}

/// `list_inputs`.
fn query_inputs(state: &State, query: &AgentInputQuery) -> Vec<AgentInputRecord> {
    let mut records: Vec<AgentInputRecord> = state
        .inputs
        .iter()
        .filter(|record| record.session_id == query.session_id && record.agent_id.is_none())
        .filter(|record| {
            query
                .target_task_id
                .as_ref()
                .is_none_or(|target| record.target_task_id.as_ref() == Some(target))
        })
        .filter(|record| {
            query
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&record.kind))
        })
        .filter(|record| {
            query
                .statuses
                .as_ref()
                .is_none_or(|statuses| statuses.contains(&record.status))
        })
        .cloned()
        .collect();
    records.sort_by_key(|record| record.order);
    records
}

/// `transition_input`.
fn transition_input(
    state: &mut State,
    input_id: &str,
    from: &[AgentInputStatus],
    to: AgentInputStatus,
    patch: &AgentInputPatch,
    now_text: &str,
) -> Result<AgentInputRecord> {
    let mut record = state.required_input(input_id)?;
    assert_transition(&record, from, to)?;
    record.status = to;
    if patch.applied_task_id.is_some() {
        record.applied_task_id = patch.applied_task_id.clone();
    }
    if patch.applied_message_id.is_some() {
        record.applied_message_id = patch.applied_message_id.clone();
    }
    if patch.answer_id.is_some() {
        record.answer_id = patch.answer_id.clone();
    }
    record.updated_at = now_text.into();
    let slot = state
        .inputs
        .iter_mut()
        .find(|slot| slot.input_id == input_id)
        .expect("present");
    *slot = record.clone();
    Ok(record)
}

// ---------------------------------------------------------------------------
// Task insert and turn staging — `tasks::insert`, `convert_claimed`.
// ---------------------------------------------------------------------------

/// `tasks::insert` (unbound): duplicate/identity gates, the exclusive-scope
/// refusal, dependency existence, and the queued record.
fn insert_task(state: &mut State, input: SubmitTask) -> Result<TaskRecord> {
    if state.task(&input.task_id).is_some() {
        return Err(Fault::Duplicate(input.task_id));
    }
    if input.task_id.is_empty() || input.scope_id.is_empty() || input.kind.is_empty() {
        return Err(Fault::InvalidInput(
            "task id, scope, and kind must be nonempty".into(),
        ));
    }
    if input.exclusive_scope
        && state
            .tasks
            .iter()
            .any(|task| task.scope_id == input.scope_id && !task.status.is_terminal())
    {
        return Err(Fault::ScopeBusy(input.scope_id));
    }
    for dependency in input.depends_on.iter().chain(input.parent_task_id.iter()) {
        if state.task(dependency).is_none() {
            return Err(Fault::TaskInput {
                reason: TaskInputReason::UnknownDependency,
                message: format!("unknown dependency: {dependency}"),
            });
        }
    }
    let task = TaskRecord {
        task_id: input.task_id,
        kind: input.kind,
        scope_id: input.scope_id,
        status: TaskStatus::Queued,
        payload: input.payload,
        priority: input.priority,
        enqueued_at: input.now,
        available_at: input.available_at.unwrap_or(input.now),
        started_at: None,
        finished_at: None,
        depends_on: input.depends_on,
        parent_task_id: input.parent_task_id,
        attempt_count: 0,
        poison_count: 0,
        dead_lettered_at: None,
        dead_letter_reason: None,
        error: None,
        progress: None,
        cancel_requested: false,
        runtime_state: None,
        retry_pending: false,
        interrupt_requested: false,
        agent_id: None,
        agent_revision: None,
        admission_key: None,
        run_profile: None,
    };
    state.tasks.push(task.clone());
    Ok(task)
}

fn usable_user_content(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.is_empty(),
        Value::Array(blocks) => !blocks.is_empty(),
        _ => false,
    }
}

/// `tasks::validate_projection`.
fn validate_projection(task: &TaskRecord, message: &MessageInput) -> Result<()> {
    if message.chat_id != task.scope_id || message.run_id.as_deref() != Some(&task.task_id) {
        return Err(Fault::InvalidInput(
            "message scope/run does not match task".into(),
        ));
    }
    Ok(())
}

/// `session_ops::convert_claimed`: the Pending -> Converted CAS is the claim,
/// and the task and its messages land only behind the receipt.
fn convert_claimed(
    state: &mut State,
    record: &AgentInputRecord,
    candidate: &StagedCandidate,
    now_text: &str,
    now: i64,
) -> Result<()> {
    let user_message_id = (record.kind == AgentInputKind::Steer)
        .then(|| {
            candidate
                .messages
                .iter()
                .find(|message| message.role == "user")
                .and_then(|message| message.id.clone())
        })
        .flatten();
    transition_input(
        state,
        &candidate.input_id,
        &[AgentInputStatus::Pending],
        AgentInputStatus::Converted,
        &AgentInputPatch {
            applied_task_id: Some(candidate.task.task_id.clone()),
            applied_message_id: user_message_id,
            ..Default::default()
        },
        now_text,
    )?;
    let mut submit = candidate.task.clone();
    submit.now = now;
    let task = insert_task(state, submit)?;
    for message in &candidate.messages {
        if message.role == "user" {
            if message.chat_id != candidate.task.scope_id {
                return Err(Fault::InvalidInput(
                    "A turn may only append messages to its own chat.".into(),
                ));
            }
            if !usable_user_content(&message.content) {
                return Err(Fault::InvalidInput(
                    "Projected user messages must carry user content.".into(),
                ));
            }
        } else {
            validate_projection(&task, message)?;
        }
        append_message(state, message.clone(), now)?;
    }
    Ok(())
}

/// `session_ops::convert_candidate`: false (writing nothing) when the input
/// is gone or no longer Pending in this session; a busy scope forfeits only
/// this candidate, mirroring the store's savepoint.
fn convert_candidate(
    state: &mut State,
    scope_id: &str,
    candidate: &StagedCandidate,
    now: i64,
) -> Result<bool> {
    let Some(record) = state.input(&candidate.input_id) else {
        return Ok(false);
    };
    if record.status != AgentInputStatus::Pending || record.session_id != scope_id {
        return Ok(false);
    }
    let mut scratch = state.clone();
    let now_text = text(now)?;
    match convert_claimed(&mut scratch, &record, candidate, &now_text, now) {
        Ok(()) => {
            *state = scratch;
            Ok(true)
        }
        // The savepoint rollback is what makes swallowing ScopeBusy sound.
        Err(Fault::ScopeBusy(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// `session_ops::unrelated_live_task`: a running/parked/interrupted task of
/// ANOTHER kind in the scope cannot promise to promote a queued input.
fn unrelated_live_task(
    state: &State,
    session_id: &str,
    task_kind: &str,
    exclude_task_id: Option<&str>,
) -> bool {
    state.tasks.iter().any(|task| {
        task.scope_id == session_id
            && task.kind != task_kind
            && exclude_task_id.is_none_or(|excluded| task.task_id != excluded)
            && matches!(
                task.status,
                TaskStatus::Running | TaskStatus::Interrupted | TaskStatus::WaitingApproval
            )
    })
}

// ---------------------------------------------------------------------------
// Chat metadata helpers — `session_ops`' standing-grant and answer rules.
// ---------------------------------------------------------------------------

fn writable_standing(metadata: &Value, session_id: &str) -> Result<Vec<Value>> {
    writable_standing_grants(metadata).ok_or_else(|| {
        Fault::InvalidInput(format!(
            "The session's standing approvals are unreadable and were not overwritten. ({session_id})"
        ))
    })
}

fn make_room(grants: Vec<Value>, session_id: &str) -> Result<Vec<Value>> {
    agentkit_sessions::make_room(grants).ok_or_else(|| {
        Fault::InvalidInput(format!(
            "A session keeps at most {} standing approvals; revoke one first. ({session_id})",
            agentkit_sessions::MAX_STANDING_APPROVALS
        ))
    })
}

fn set_metadata(state: &mut State, chat: &mut ChatRecord, metadata: Value, now: i64) -> Result<()> {
    chat.metadata = metadata;
    chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
    state.save_chat(chat);
    Ok(())
}

fn request_tool_name(content: &Value) -> String {
    content
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("request_user_input")
        .to_owned()
}

fn stale(request_id: &str, reason: &str) -> Fault {
    Fault::AnswerStale(format!(
        "Request {request_id} can no longer be answered: {reason}."
    ))
}

fn answer_envelope(response: &Value) -> String {
    json!({
        "ok": true, "status": "ok", "summary": "The user answered.",
        "warnings": [], "truncated": false,
        "data": {"answer": response},
    })
    .to_string()
}

fn deny_envelope(note: &Option<String>) -> String {
    let message = note
        .clone()
        .unwrap_or_else(|| "The user denied this tool call.".into());
    json!({
        "ok": false, "status": "error", "summary": "The user denied this tool call.",
        "warnings": [], "truncated": false,
        "data": {
            "errorCode": "approval_denied",
            "errorMessage": message,
            "phase": "guard",
            "retryable": false,
        },
    })
    .to_string()
}

/// `conversations::order::order_messages_for_provider`: within one run's
/// slot, a tool result moves next to the internal assistant record that
/// called it, conversation order between runs preserved.
fn order_messages_for_provider(mut records: Vec<MessageRecord>) -> Vec<MessageRecord> {
    records.sort_by_key(|record| record.order_key);
    let mut slots: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (position, record) in records.iter().enumerate() {
        if record.role == "user" {
            continue;
        }
        if let Some(run) = &record.run_id {
            slots.entry(run.clone()).or_default().push(position);
        }
    }
    let mut ordered = records.clone();
    for positions in slots.values() {
        let run = order_run(
            positions
                .iter()
                .map(|index| records[*index].clone())
                .collect(),
        );
        for (position, record) in positions.iter().zip(run) {
            ordered[*position] = record;
        }
    }
    ordered
}

fn is_prefix(record: &MessageRecord) -> bool {
    record.role == "tool"
        || (record.role == "assistant"
            && record.metadata.get("internal") == Some(&Value::Bool(true)))
}

fn order_run(records: Vec<MessageRecord>) -> Vec<MessageRecord> {
    if records
        .iter()
        .any(|record| record.metadata.get("canonicalProviderTurn") == Some(&Value::Bool(true)))
    {
        return records;
    }
    let mut tools: std::collections::HashMap<String, Vec<MessageRecord>> =
        std::collections::HashMap::new();
    for record in records.iter().filter(|record| record.role == "tool") {
        if let Some(call) = &record.tool_call_id {
            tools.entry(call.clone()).or_default().push(record.clone());
        }
    }
    let mut claimed = std::collections::HashSet::new();
    let mut groups: std::collections::HashMap<String, Vec<MessageRecord>> =
        std::collections::HashMap::new();
    for record in records.iter().filter(|record| {
        record.role == "assistant" && record.metadata.get("internal") == Some(&Value::Bool(true))
    }) {
        let mut results = Vec::new();
        for call in record
            .tool_calls
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = call.get("id").and_then(Value::as_str)
                && let Some(group) = tools.remove(id)
            {
                for result in &group {
                    claimed.insert(result.id.clone());
                }
                results.extend(group);
            }
        }
        groups.insert(record.id.clone(), results);
    }
    let mut ordered = Vec::new();
    for record in records
        .iter()
        .filter(|record| is_prefix(record) && !claimed.contains(&record.id))
    {
        ordered.push(record.clone());
        if let Some(group) = groups.remove(&record.id) {
            ordered.extend(group);
        }
    }
    ordered.extend(records.into_iter().filter(|record| !is_prefix(record)));
    ordered
}

/// The host-authorized plan grant, mirroring `session_ops::PlanGrant`.
struct PlanGrant<'a> {
    session_id: &'a str,
    request_id: &'a str,
    turn_id: &'a str,
    plan: &'a Value,
    granted_mode: &'a str,
}

/// The plan grant of `session_ops::apply_plan_grant`, one authoritative write.
fn apply_plan_grant(
    state: &mut State,
    grant: &PlanGrant<'_>,
    now_text: &str,
    now: i64,
) -> Result<()> {
    let PlanGrant {
        session_id,
        request_id,
        turn_id,
        plan,
        granted_mode,
    } = *grant;
    if granted_mode == "plan" || granted_mode.is_empty() {
        return Err(Fault::InvalidInput(format!(
            "Plan approval cannot grant plan mode. ({request_id})"
        )));
    }
    let mut chat = state
        .chat(session_id)
        .map_err(|_| Fault::NotFound(format!("Session {session_id} has no chat to grant.")))?;
    let prior = read_execution_state(&chat.metadata);
    let mut grant_record = serde_json::Map::new();
    grant_record.insert("grantId".into(), format!("grant:{request_id}").into());
    grant_record.insert(
        "planDigest".into(),
        plan.get("planDigest")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
    );
    if let Some(plan_text) = plan.get("plan").and_then(Value::as_str) {
        grant_record.insert("plan".into(), plan_text.into());
    }
    grant_record.insert("mode".into(), granted_mode.into());
    if let Some(mode) = plan.get("requestedMode").and_then(Value::as_str) {
        grant_record.insert("requestedMode".into(), mode.into());
    }
    grant_record.insert("requestId".into(), request_id.into());
    grant_record.insert("turnId".into(), turn_id.into());
    grant_record.insert("approvedAt".into(), now_text.into());
    let grant = Value::Object(grant_record);
    let state_json = execution_state_for_grant(&grant, now_text, prior).ok_or_else(|| {
        Fault::InvalidInput(format!(
            "Plan approval grant is invalid: mode \"plan\" is never grantable and planDigest is required. ({request_id})"
        ))
    })?;
    let metadata = with_execution(&chat.metadata, Some(state_json));
    set_metadata(state, &mut chat, metadata, now)
}

/// `session_ops::write_request_grant` + `write_standing_grant`.
fn write_request_grant(
    state: &mut State,
    request: &AgentInputRecord,
    answer_id: &str,
    now_text: &str,
    now: i64,
) -> Result<()> {
    let Some((digest, label)) = read_standing_key(&request.content) else {
        return Err(Fault::InvalidInput(format!(
            "This call is not eligible for a standing approval. ({})",
            request.input_id
        )));
    };
    let grant = json!({
        "grantId": format!("grant:{}", request.input_id),
        "digest": digest,
        "label": label,
        "toolName": request_tool_name(&request.content),
        "sourceRequestId": request.input_id,
        "answerId": answer_id,
        "grantedAt": now_text,
    });
    let mut chat = state.chat(&request.session_id)?;
    let grants = writable_standing(&chat.metadata, &request.session_id)?;
    let grant_id = grant.get("grantId").and_then(Value::as_str);
    if grants
        .iter()
        .any(|existing| existing.get("grantId").and_then(Value::as_str) == grant_id)
    {
        return Ok(());
    }
    let mut next = make_room(grants, &request.session_id)?;
    next.push(grant);
    let metadata = with_standing(&chat.metadata, next);
    set_metadata(state, &mut chat, metadata, now)
}

/// What `session_ops::AppendAnswer` carries: the tool record one answer appends.
struct AppendAnswer<'a> {
    id: &'a str,
    request: &'a AgentInputRecord,
    task_id: &'a str,
    parent_message_id: &'a str,
    envelope_json: &'a str,
    answer_id: &'a str,
    response_hash: &'a str,
}

/// The tool record a non-approval answer appends (`session_ops::append_answer`).
fn append_answer(state: &mut State, input: &AppendAnswer<'_>, now: i64) -> Result<()> {
    let AppendAnswer {
        id,
        request,
        task_id,
        parent_message_id,
        envelope_json,
        answer_id,
        response_hash,
    } = *input;
    append_message(
        state,
        MessageInput {
            id: Some(id.to_owned()),
            chat_id: request.session_id.clone(),
            run_id: Some(task_id.to_owned()),
            role: "tool".into(),
            content: Value::String(envelope_json.to_owned()),
            parent_message_id: Some(parent_message_id.to_owned()),
            tool_call_id: request.tool_call_id.clone(),
            model_result_json: Some(envelope_json.to_owned()),
            metadata: json!({
                "internal": true,
                "toolName": request_tool_name(&request.content),
                "humanAnswer": true,
                "answerId": answer_id,
                "requestId": request.input_id,
                "responseHash": response_hash,
            }),
            activate: false,
            ..MessageInput::default()
        },
        now,
    )?;
    Ok(())
}

/// `session_ops::requeue_task`.
fn requeue_task(state: &mut State, task: &TaskRecord, now: i64) {
    let mut parked = task.clone();
    parked.status = TaskStatus::Queued;
    parked.available_at = now;
    parked.finished_at = None;
    state.save_task(&parked);
}

// ---------------------------------------------------------------------------
// The answer transaction — `session_ops::answer_human_request`.
// ---------------------------------------------------------------------------

fn read_decision(response: &Value) -> Result<agentkit_sessions::Decision> {
    agentkit_sessions::read_decision(response).map_err(Fault::InvalidInput)
}

fn enqueue_if_queued(task: &Option<TaskRecord>) -> Option<(String, String)> {
    task.as_ref()
        .filter(|task| task.status == TaskStatus::Queued)
        .map(|task| (task.task_id.clone(), task.scope_id.clone()))
}

fn recorded_response_hash(state: &State, message_id: Option<&str>) -> Option<String> {
    let id = message_id?;
    state
        .messages
        .iter()
        .find(|message| message.id == id)
        .and_then(|message| message.metadata.get("responseHash"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

impl State {
    /// Records one human answer and re-admits the parked turn, all in one
    /// lock: the same ordering, staleness gates and duplicate receipts the
    /// SQLite transaction has.
    fn answer(&mut self, input: &AnswerHumanRequest) -> Result<AnswerOutcome> {
        let now_text = text(input.now)?;
        let request = self
            .input(&input.request_id)
            .filter(|record| record.kind == AgentInputKind::HumanRequest)
            .ok_or_else(|| {
                Fault::NotFound(format!("Human request not found: {}", input.request_id))
            })?;
        let approval0 = if is_approval_request(&request.content) {
            Some(read_decision(&input.response)?)
        } else {
            None
        };
        let approval = match (&approval0, &input.policy_denial) {
            (Some(decision), None) => Some(decision.clone()),
            (Some(_), Some(reason)) => Some(agentkit_sessions::Decision {
                approve: false,
                note: Some(format!("plan_not_authorized: {reason}")),
                remember: None,
            }),
            (None, _) => None,
        };
        let envelope_json = match &approval {
            Some(decision) if !decision.approve => deny_envelope(&decision.note),
            _ => answer_envelope(&input.response),
        };
        let task_id = request
            .target_task_id
            .clone()
            .ok_or_else(|| stale(&input.request_id, "it names no turn"))?;
        let task = self.task(&task_id);
        let marker = format!(
            "{}{}",
            agentkit_sessions::APPROVE_MARKER,
            input.response_hash
        );
        let answered = request.status == AgentInputStatus::Answered;
        let approves = request
            .applied_message_id
            .as_deref()
            .is_some_and(|id| id.starts_with(agentkit_sessions::APPROVE_MARKER));
        if answered && approves {
            // An approve has no tool record: the marker carries the hash.
            if request.answer_id.as_deref() != Some(input.answer_id.as_str())
                || request.applied_message_id.as_deref() != Some(marker.as_str())
            {
                return Err(Fault::AnswerConflict(format!(
                    "answer_conflict: Request {} was already answered differently.",
                    input.request_id
                )));
            }
            return Ok(AnswerOutcome {
                receipt: AnswerReceipt {
                    request_id: input.request_id.clone(),
                    answer_id: input.answer_id.clone(),
                    turn_id: task_id,
                    message_id: String::new(),
                    duplicate: true,
                },
                enqueue: enqueue_if_queued(&task),
            });
        }
        if answered {
            if request.answer_id.as_deref() != Some(input.answer_id.as_str())
                || recorded_response_hash(self, request.applied_message_id.as_deref()).as_deref()
                    != Some(input.response_hash.as_str())
            {
                return Err(Fault::AnswerConflict(format!(
                    "answer_conflict: Request {} was already answered differently.",
                    input.request_id
                )));
            }
            return Ok(AnswerOutcome {
                receipt: AnswerReceipt {
                    request_id: input.request_id.clone(),
                    answer_id: input.answer_id.clone(),
                    turn_id: task_id,
                    message_id: request.applied_message_id.clone().unwrap_or_default(),
                    duplicate: true,
                },
                enqueue: enqueue_if_queued(&task),
            });
        }
        if request.status == AgentInputStatus::Cancelled {
            return Err(stale(&input.request_id, "it was cancelled"));
        }
        let task = task
            .filter(|task| task.status == TaskStatus::WaitingApproval)
            .ok_or_else(|| stale(&input.request_id, "its turn is no longer waiting"))?;
        if request
            .expires_at
            .as_deref()
            .and_then(parse_answer_expiry)
            .is_some_and(|expires| expires <= input.now)
        {
            return Err(stale(&input.request_id, "it expired"));
        }
        let approves = approval.as_ref().is_some_and(|decision| decision.approve);
        if approves {
            // `session_ops.rs:361` — the standing-eligibility refusal belongs
            // to a `remember: true` approval alone; a plain approve never
            // touches the standing-grant namespace.
            if input_response_remember(input) {
                self.check_remember_room(&request, input)?;
            }
            let outcome =
                self.commit_approval(&request, input, &task, &task_id, &marker, &now_text);
            return outcome.map_err(|error| match error {
                Fault::AgentInputTransition(_) => {
                    stale(&input.request_id, "a concurrent answer or cancel won")
                }
                other => other,
            });
        }
        let last = self
            .last_message_of_run(&request.session_id, &task_id)
            .ok_or_else(|| Fault::NotFound(format!("Turn {task_id} has no run records.")))?;
        transition_input(
            self,
            &input.request_id,
            &[AgentInputStatus::Pending],
            AgentInputStatus::Answered,
            &AgentInputPatch {
                answer_id: Some(input.answer_id.clone()),
                applied_message_id: Some(input.message_id.clone()),
                ..Default::default()
            },
            &now_text,
        )
        .map_err(|error| match error {
            Fault::AgentInputTransition(_) => {
                stale(&input.request_id, "a concurrent answer or cancel won")
            }
            other => other,
        })?;
        requeue_task(self, &task, input.now);
        append_answer(
            self,
            &AppendAnswer {
                id: &input.message_id,
                request: &request,
                task_id: &task_id,
                parent_message_id: &last.id,
                envelope_json: &envelope_json,
                answer_id: &input.answer_id,
                response_hash: &input.response_hash,
            },
            input.now,
        )?;
        Ok(AnswerOutcome {
            receipt: AnswerReceipt {
                request_id: input.request_id.clone(),
                answer_id: input.answer_id.clone(),
                turn_id: task_id.clone(),
                message_id: input.message_id.clone(),
                duplicate: false,
            },
            enqueue: Some((task_id, task.scope_id)),
        })
    }

    /// The refusal-before-committing half of a `remember: true` approval.
    fn check_remember_room(
        &mut self,
        request: &AgentInputRecord,
        input: &AnswerHumanRequest,
    ) -> Result<()> {
        if read_standing_key(&request.content).is_none() {
            return Err(Fault::InvalidInput(format!(
                "This call is not eligible for a standing approval. ({})",
                input.request_id
            )));
        }
        let grant_id = format!("grant:{}", input.request_id);
        let chat = self.chat(&request.session_id)?;
        let grants = writable_standing(&chat.metadata, &request.session_id)?;
        if !grants
            .iter()
            .any(|grant| grant.get("grantId").and_then(Value::as_str) == Some(grant_id.as_str()))
        {
            make_room(grants, &request.session_id)?;
        }
        Ok(())
    }

    /// The approve leg's writes, in the store's order: task transition,
    /// the answer's CAS, the plan grant, then the standing grant.
    fn commit_approval(
        &mut self,
        request: &AgentInputRecord,
        input: &AnswerHumanRequest,
        task: &TaskRecord,
        task_id: &str,
        marker: &str,
        now_text: &str,
    ) -> Result<AnswerOutcome> {
        let remember = input_response_remember(input);
        requeue_task(self, task, input.now);
        transition_input(
            self,
            &input.request_id,
            &[AgentInputStatus::Pending],
            AgentInputStatus::Answered,
            &AgentInputPatch {
                answer_id: Some(input.answer_id.clone()),
                applied_message_id: Some(marker.to_owned()),
                ..Default::default()
            },
            now_text,
        )?;
        let plan = plan_request_of(&request.content, &input.plan_tool_names);
        if let (Some(plan), Some(granted_mode)) = (plan.as_ref(), input.granted_mode.as_deref()) {
            apply_plan_grant(
                self,
                &PlanGrant {
                    session_id: &request.session_id,
                    request_id: &input.request_id,
                    turn_id: task_id,
                    plan: &plan.clone(),
                    granted_mode,
                },
                now_text,
                input.now,
            )?;
        }
        if remember {
            write_request_grant(self, request, &input.answer_id, now_text, input.now)?;
        }
        Ok(AnswerOutcome {
            receipt: AnswerReceipt {
                request_id: input.request_id.clone(),
                answer_id: input.answer_id.clone(),
                turn_id: task_id.to_owned(),
                message_id: String::new(),
                duplicate: false,
            },
            enqueue: Some((task_id.to_owned(), task.scope_id.clone())),
        })
    }
}

/// `remember: true` of the caller's own decision, the only shape that
/// writes a standing grant.
fn input_response_remember(input: &AnswerHumanRequest) -> bool {
    let Ok(decision) = read_decision(&input.response) else {
        return false;
    };
    input.policy_denial.is_none() && decision.approve && decision.remember == Some(true)
}

// ---------------------------------------------------------------------------
// The catalog validators — `provider_catalog`'s shape gates.
// ---------------------------------------------------------------------------

fn invalid_catalog(kind: &str) -> Fault {
    Fault::RecoveryUnreadable(format!("Malformed {kind} record"))
}

fn required_strings(value: &Value, fields: &[&str]) -> bool {
    value.is_object() && fields.iter().all(|field| value[*field].is_string())
}

/// `provider_catalog::provider`.
fn is_provider(value: &Value) -> bool {
    required_strings(value, &["id", "label", "kind", "baseUrl", "defaultModel"])
        && value["enabled"].is_boolean()
        && value.get("apiKey").is_none_or(Value::is_string)
        && value.get("metadata").is_none_or(Value::is_object)
        && value.get("extraHeaders").is_none_or(|headers| {
            headers.is_object()
                && headers
                    .as_object()
                    .is_some_and(|map| map.values().all(Value::is_string))
        })
}

/// `provider_catalog::capabilities`.
fn is_capabilities(value: &Value) -> bool {
    value.is_object()
        && ["streaming", "toolCalling", "modelList"]
            .iter()
            .all(|field| value[*field].is_boolean())
        && ["vision", "audio", "jsonMode"]
            .iter()
            .all(|field| value.get(*field).is_none_or(Value::is_boolean))
        && value.get("maxContextTokens").is_none_or(Value::is_number)
        && ["checkedAt", "warning"]
            .iter()
            .all(|field| value.get(*field).is_none_or(Value::is_string))
}

/// `provider_catalog::model`.
fn is_model(value: &Value) -> bool {
    required_strings(value, &["providerId", "modelId", "fetchedAt"])
        && value
            .get("displayName")
            .is_some_and(|name| name.is_string() || name.is_null())
        && value
            .get("contextWindowTokens")
            .is_none_or(Value::is_number)
        && value
            .get("supportsToolCalling")
            .is_none_or(Value::is_boolean)
}

/// `context_summaries::valid`.
fn is_summary(value: &Value) -> bool {
    value.is_object()
        && [
            "summaryId",
            "sessionId",
            "coversThroughMessageId",
            "firstKeptMessageId",
            "summary",
            "sourceHash",
            "model",
            "summarizerRevision",
            "createdAt",
        ]
        .iter()
        .all(|field| value[*field].is_string())
        && value["tokensBefore"].is_number()
        && value["tokensAfter"].is_number()
        && value.get("providerId").is_none_or(Value::is_string)
        && value["details"]["readFiles"]
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string))
        && value["details"]["modifiedFiles"]
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string))
}

/// `managed_content::skill_entry`: the catalog projection, no document body.
fn skill_entry(record: &Value) -> Value {
    json!({
        "id": record["id"],
        "revision": record["revision"],
        "sourceId": record["sourceId"],
        "path": record["path"],
        "name": record["name"],
        "description": record["description"],
        "connectorRequirements": record["connectorRequirements"],
    })
}

// ---------------------------------------------------------------------------
// The run-profile check-at-use — `run_profiles::verify_run_profile`.
// ---------------------------------------------------------------------------

/// `run_profiles::refused`.
fn refused(reason: impl std::fmt::Display) -> Fault {
    Fault::InvalidInput(format!("run_profile_refused:{reason}"))
}

/// The references one profile document makes, as `profile_refs` reads them.
struct ProfileRefs {
    agent_id: String,
    provider_id: String,
    host_authority: bool,
    skills: Vec<(String, String)>,
    resource_sources: Vec<String>,
    approvals: Vec<(String, String, String)>,
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// `run_profiles::profile_refs`, reduced to what the dispatch verdict reads.
fn profile_refs(profile: &Value) -> Result<ProfileRefs> {
    let provider = &profile["provider"];
    // A missing authority means the catalogue (backward compatible).
    let host_authority = match provider.get("authority") {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) if text == "catalog" => false,
        Some(Value::String(text)) if text == "host" => true,
        _ => return Err(refused("provider.authority must be catalog or host")),
    };
    let list = |field: &str, keys: &[&str]| -> Vec<Vec<String>> {
        profile[field]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|entry| {
                keys.iter()
                    .filter_map(|key| entry.get(*key).and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .collect()
    };
    let Some(agent_id) = str_field(profile, "agentId") else {
        return Err(refused("agentId"));
    };
    let Some(provider_id) = str_field(provider, "providerId") else {
        return Err(refused("provider.providerId"));
    };
    Ok(ProfileRefs {
        agent_id,
        provider_id,
        host_authority,
        skills: list("skills", &["id", "revision"])
            .into_iter()
            .filter_map(|pair| Some((pair.first()?.clone(), pair.get(1)?.clone())))
            .collect(),
        resource_sources: list("contextSources", &["id", "kind"])
            .into_iter()
            .filter(|pair| pair.get(1).map(String::as_str) == Some("resource"))
            .filter_map(|pair| pair.first().cloned())
            .collect(),
        approvals: list("approvals", &["sessionId", "grantId", "digest"])
            .into_iter()
            .filter_map(|triple| {
                Some((
                    triple.first()?.clone(),
                    triple.get(1)?.clone(),
                    triple.get(2)?.clone(),
                ))
            })
            .collect(),
    })
}

/// `run_profiles::verify_run_profile`: the profile still names the facts it
/// was resolved against. `agent_id` is the task's own binding.
fn verify_run_profile(
    state: &State,
    agent_id: Option<&str>,
    profile: &Value,
    digest: &str,
) -> Result<()> {
    if run_profile_digest(profile) != digest {
        return Err(refused("digest_mismatch"));
    }
    let refs = profile_refs(profile)?;
    if agent_id != Some(refs.agent_id.as_str()) {
        return Err(refused("agent_binding_mismatch"));
    }
    if !refs.host_authority {
        let provider = state
            .setting(PROVIDERS_NAMESPACE, &refs.provider_id)
            .ok_or_else(|| refused(format!("provider_deleted:{}", refs.provider_id)))?;
        if !is_provider(&provider) || provider["enabled"].as_bool() != Some(true) {
            return Err(refused(format!("provider_revoked:{}", refs.provider_id)));
        }
        let capabilities = state
            .setting(PROVIDER_CAPABILITIES_NAMESPACE, &refs.provider_id)
            .ok_or_else(|| {
                refused(format!(
                    "provider_capabilities_revoked:{}",
                    refs.provider_id
                ))
            })?;
        if !is_capabilities(&capabilities) {
            return Err(refused(format!(
                "provider_capabilities_revoked:{}",
                refs.provider_id
            )));
        }
    }
    for (id, revision) in &refs.skills {
        let stored = state
            .skill(id)
            .and_then(|record| record["revision"].as_str().map(str::to_owned));
        if stored.as_deref() != Some(revision.as_str()) {
            return Err(refused(format!("skill_revoked:{id}")));
        }
    }
    verify_agent_and_grants(state, &refs)
}

/// The agent-definition and standing-approval legs of the same verdict.
fn verify_agent_and_grants(state: &State, refs: &ProfileRefs) -> Result<()> {
    let Some(agent) = state
        .agents
        .iter()
        .find(|agent| {
            agent.agent_id == refs.agent_id
                && agent.lifecycle != agentkit_sessions::AgentLifecycle::Deleted
        })
        .cloned()
    else {
        return Err(Fault::NotFound(format!("agent {}", refs.agent_id)));
    };
    for id in &refs.resource_sources {
        if !agent
            .definition
            .resources
            .iter()
            .any(|granted| granted == id)
        {
            return Err(refused(format!("source_revoked:{id}")));
        }
    }
    for (session_id, grant_id, digest) in &refs.approvals {
        let chat = state.chats.iter().find(|chat| chat.id == *session_id);
        let active = chat.and_then(|chat| find_active_standing_grant(&chat.metadata, digest));
        match active {
            Some(grant)
                if grant.get("grantId").and_then(Value::as_str) == Some(grant_id.as_str()) => {}
            _ => return Err(refused(format!("approval_revoked:{grant_id}"))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The port.
// ---------------------------------------------------------------------------

#[async_trait]
impl agentkit_sessions::SessionRepository for InMemorySessionRepository {
    type Error = InMemorySessionRepositoryError;

    async fn create_chat(
        &self,
        chat_id: &str,
        title: Option<String>,
        metadata: Value,
        now: i64,
    ) -> Result<ChatRecord> {
        let chat_id = chat_id.to_owned();
        self.atomic(move |state| {
            if state.chats.iter().any(|chat| chat.id == chat_id) {
                return Err(Fault::Duplicate(chat_id));
            }
            let created = ConversationTimestamp::from_epoch_millis(now)?;
            let chat = ChatRecord {
                id: chat_id,
                title,
                metadata,
                archived: false,
                created_at: created.clone(),
                updated_at: created,
            };
            state.chats.push(chat.clone());
            Ok(chat)
        })
        .await
    }

    async fn get_chat(&self, chat_id: &str) -> Result<Option<ChatRecord>> {
        let chat_id = chat_id.to_owned();
        let state = self.state.lock().await;
        Ok(state.chats.iter().find(|chat| chat.id == chat_id).cloned())
    }

    async fn patch_session(
        &self,
        chat_id: &str,
        mutation: SessionMutation,
        now: i64,
    ) -> Result<ChatRecord> {
        let chat_id = chat_id.to_owned();
        self.atomic(move |state| {
            let mut chat = state.chat(&chat_id)?;
            apply_session_mutation(&mut chat, &mutation, now).map_err(|refusal| match refusal {
                SessionRefusal::NotFound(message) => Fault::NotFound(message),
                SessionRefusal::InvalidInput(message) => Fault::InvalidInput(message),
            })?;
            chat.updated_at = ConversationTimestamp::from_epoch_millis(now)?;
            state.save_chat(&chat);
            Ok(chat)
        })
        .await
    }

    async fn messages(&self, chat_id: &str, include_inactive: bool) -> Result<Vec<MessageRecord>> {
        let chat_id = chat_id.to_owned();
        let state = self.state.lock().await;
        let mut messages = state.chat_messages(&chat_id);
        messages.retain(|message| include_inactive || message.active);
        messages.sort_by_key(|message| message.order_key);
        Ok(messages)
    }

    async fn fork_chat(
        &self,
        chat_id: &str,
        from_message_id: &str,
        new_chat_id: &str,
        now: i64,
    ) -> Result<(ChatRecord, Vec<MessageRecord>)> {
        let (chat_id, from_message_id, new_chat_id) = (
            chat_id.to_owned(),
            from_message_id.to_owned(),
            new_chat_id.to_owned(),
        );
        self.atomic(move |state| fork(state, &chat_id, &from_message_id, &new_chat_id, now))
            .await
    }

    async fn list_context_summaries(&self, session: &str) -> Result<Vec<Value>> {
        let session = session.to_owned();
        let state = self.state.lock().await;
        let mut records = Vec::new();
        for (key, value) in state.namespace(CONTEXT_SUMMARIES_NAMESPACE) {
            if !is_summary(&value) || value["summaryId"].as_str() != Some(key.as_str()) {
                return Err(invalid_catalog("context summary"));
            }
            if value["sessionId"].as_str() == Some(session.as_str()) {
                records.push(value);
            }
        }
        records.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
        Ok(records)
    }

    async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
        let state = self.state.lock().await;
        Ok(state.setting(namespace, key))
    }

    async fn events(
        &self,
        task_id: &str,
        after_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>> {
        let task_id = task_id.to_owned();
        let state = self.state.lock().await;
        state.required_task(&task_id)?;
        let after = after_seq.map(|sequence| sequence as f64);
        let mut events: Vec<EventRecord> = state
            .events
            .iter()
            .filter(|event| event.task_id == task_id)
            .filter(|event| after.is_none_or(|after| event.seq > after))
            .cloned()
            .collect();
        events.sort_by(|a, b| {
            a.seq
                .partial_cmp(&b.seq)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        events.truncate(limit.min(10_000));
        Ok(events)
    }

    async fn list_tasks(&self, scope_id: Option<String>) -> Result<Vec<TaskRecord>> {
        let state = self.state.lock().await;
        Ok(match scope_id {
            Some(scope) => state
                .tasks
                .iter()
                .filter(|task| task.scope_id == scope)
                .cloned()
                .collect(),
            None => state.tasks.clone(),
        })
    }

    async fn create_input(
        &self,
        input: CreateAgentInput,
        now: i64,
    ) -> Result<(AgentInputRecord, bool)> {
        self.atomic(move |state| {
            let now_text = text(now)?;
            insert_input(state, input, &now_text)
        })
        .await
    }

    async fn get_input(&self, input_id: String) -> Result<Option<AgentInputRecord>> {
        let state = self.state.lock().await;
        Ok(state.input(&input_id))
    }

    async fn list_inputs(&self, query: AgentInputQuery) -> Result<Vec<AgentInputRecord>> {
        let state = self.state.lock().await;
        Ok(query_inputs(&state, &query))
    }

    async fn transition_input(
        &self,
        input_id: String,
        from: Vec<AgentInputStatus>,
        to: AgentInputStatus,
        patch: AgentInputPatch,
        now: i64,
    ) -> Result<AgentInputRecord> {
        self.atomic(move |state| {
            let now_text = text(now)?;
            transition_input(state, &input_id, &from, to, &patch, &now_text)
        })
        .await
    }

    async fn queue_or_stage_turn(&self, input: QueueOrStage) -> Result<QueueAdmission> {
        self.atomic(move |state| queue_or_stage(state, input)).await
    }

    async fn stage_queued_turn(&self, input: StageQueuedTurn) -> Result<Option<String>> {
        self.atomic(move |state| stage_queued(state, input)).await
    }

    async fn answer_human_request(&self, input: AnswerHumanRequest) -> Result<AnswerOutcome> {
        self.atomic(move |state| state.answer(&input)).await
    }

    async fn request_interrupt(&self, task_id: String, now: i64) -> Result<()> {
        self.atomic(move |state| {
            let mut task = state
                .task(&task_id)
                .ok_or_else(|| Fault::NotFound(format!("Task {task_id} does not exist.")))?;
            if task.status.is_terminal() {
                return Ok(());
            }
            match task.status {
                TaskStatus::Queued => {
                    task.status = TaskStatus::Interrupted;
                    state.save_task(&task);
                }
                TaskStatus::Running if !task.cancel_requested => {
                    task.interrupt_requested = true;
                    state.save_task(&task);
                }
                _ => {}
            }
            let _ = now;
            Ok(())
        })
        .await
    }

    async fn request_compact(
        &self,
        session_id: String,
        task_kind: String,
        expected_descriptor: Value,
        now: i64,
    ) -> Result<()> {
        self.atomic(move |state| {
            if state.tasks.iter().any(|task| {
                task.scope_id == session_id && task.kind == task_kind && live(task.status)
            }) {
                return Err(Fault::ScopeBusy(session_id));
            }
            let mut chat = state.chat(&session_id)?;
            if chat.metadata.get(SESSION_METADATA_KEY) != Some(&expected_descriptor) {
                return Err(Fault::SessionDescriptorChanged(session_id));
            }
            let mut metadata = chat.metadata.as_object().cloned().unwrap_or_default();
            metadata.insert(
                agentkit_sessions::COMPACT_REQUESTED_METADATA_KEY.into(),
                Value::from(text(now)?),
            );
            set_metadata(state, &mut chat, Value::Object(metadata), now)
        })
        .await
    }

    async fn revoke_standing_approval(
        &self,
        session_id: String,
        grant_id: String,
        now: i64,
    ) -> Result<Value> {
        self.atomic(move |state| {
            let mut chat = state.chat(&session_id)?;
            let mut grants = writable_standing(&chat.metadata, &session_id)?;
            let Some(index) = grants.iter().position(|grant| {
                grant.get("grantId").and_then(Value::as_str) == Some(grant_id.as_str())
            }) else {
                return Err(Fault::NotFound(format!(
                    "Standing approval not found: {grant_id} ({session_id})"
                )));
            };
            if grants[index]
                .get("revokedAt")
                .is_some_and(|value| !value.is_null())
            {
                return Ok(grants[index].clone());
            }
            grants[index]["revokedAt"] = Value::from(text(now)?);
            let metadata = with_standing(&chat.metadata, grants.clone());
            set_metadata(state, &mut chat, metadata, now)?;
            Ok(grants[index].clone())
        })
        .await
    }

    async fn submit_turn(
        &self,
        input: SubmitTask,
        messages: Vec<MessageInput>,
    ) -> Result<TaskRecord> {
        self.atomic(move |state| submit_turn(state, input, messages))
            .await
    }

    async fn get_agent(&self, agent_id: String) -> Result<Option<AgentRecord>> {
        let state = self.state.lock().await;
        // A tombstone reads as absence, exactly like the store's live read.
        Ok(state
            .agents
            .iter()
            .find(|agent| agent.agent_id == agent_id)
            .filter(|agent| agent.lifecycle != agentkit_sessions::AgentLifecycle::Deleted)
            .cloned())
    }

    async fn get_provider(&self, id: &str) -> Result<Option<Value>> {
        let id = id.to_owned();
        let state = self.state.lock().await;
        let value = state.setting(PROVIDERS_NAMESPACE, &id);
        if value
            .as_ref()
            .is_some_and(|value| !is_provider(value) || value["id"].as_str() != Some(id.as_str()))
        {
            return Err(invalid_catalog("provider"));
        }
        Ok(value)
    }

    async fn get_provider_capabilities(&self, id: &str) -> Result<Option<Value>> {
        let id = id.to_owned();
        let state = self.state.lock().await;
        let value = state.setting(PROVIDER_CAPABILITIES_NAMESPACE, &id);
        if value.as_ref().is_some_and(|value| !is_capabilities(value)) {
            return Err(invalid_catalog("provider capabilities"));
        }
        Ok(value)
    }

    async fn list_provider_models(&self, id: &str) -> Result<Vec<Value>> {
        let id = id.to_owned();
        let state = self.state.lock().await;
        let Some(value) = state.setting(PROVIDER_MODELS_NAMESPACE, &id) else {
            return Ok(vec![]);
        };
        let mut models = value
            .as_array()
            .ok_or_else(|| invalid_catalog("provider models"))?
            .clone();
        let mut seen = std::collections::HashSet::new();
        if models.iter().any(|item| {
            !is_model(item) || !seen.insert(item["modelId"].as_str().unwrap_or_default().to_owned())
        }) {
            return Err(invalid_catalog("provider models"));
        }
        models.sort_by(|a, b| a["modelId"].as_str().cmp(&b["modelId"].as_str()));
        Ok(models)
    }

    async fn get_skill(&self, id: String) -> Result<Option<Value>> {
        let state = self.state.lock().await;
        Ok(state.skill(&id).map(|record| skill_entry(&record)))
    }

    async fn resolve_run_content(
        &self,
        selection: Value,
        authorized_connectors: Vec<String>,
        tolerate_unavailable: bool,
    ) -> Result<Value> {
        self.atomic(move |state| {
            resolve_run_content(
                state,
                &selection,
                &authorized_connectors,
                tolerate_unavailable,
            )
        })
        .await
    }

    async fn run_profile_dispatch_refusal(&self, task_id: String) -> Result<Option<String>> {
        let state = self.state.lock().await;
        let task = state.required_task(&task_id)?;
        let Some(pin) = task.run_profile.as_ref() else {
            return Ok(None);
        };
        Ok(
            verify_run_profile(&state, task.agent_id.as_deref(), &pin.profile, &pin.digest)
                .err()
                .map(|error| error.to_string()),
        )
    }

    async fn list_executions_by_chat(
        &self,
        chat_id: String,
    ) -> Result<Vec<ExecutionReceiptRecord>> {
        let state = self.state.lock().await;
        Ok(state
            .receipts
            .iter()
            .filter(|receipt| receipt.chat_id == chat_id)
            .cloned()
            .collect())
    }

    async fn resolve_execution(
        &self,
        execution_id: String,
        request: ResolveExecution,
    ) -> Result<ExecutionReceiptRecord> {
        self.atomic(move |state| resolve_execution(state, &execution_id, request))
            .await
    }
}

// ---------------------------------------------------------------------------
// The compound writes.
// ---------------------------------------------------------------------------

/// `conversations::history::fork_chat`: the source path up to (and
/// including) the fork point is copied, placeholders dropped, and neither
/// pending inputs nor a continuation travel.
fn fork(
    state: &mut State,
    chat_id: &str,
    from_message_id: &str,
    new_chat_id: &str,
    now: i64,
) -> Result<(ChatRecord, Vec<MessageRecord>)> {
    let source = state.chat(chat_id)?;
    let mut cursor = state
        .messages
        .iter()
        .find(|message| message.id == from_message_id)
        .filter(|message| message.chat_id == *chat_id && message.active)
        .cloned()
        .ok_or_else(|| Fault::TaskInput {
            reason: TaskInputReason::InvalidForkPoint,
            message: "invalid_fork_point".into(),
        })?;
    let mut path = vec![cursor.clone()];
    while let Some(parent) = cursor.parent_message_id {
        cursor = state
            .chat_messages(chat_id)
            .into_iter()
            .find(|message| message.id == parent)
            .ok_or_else(|| Fault::NotFound(parent.clone()))?;
        path.push(cursor.clone());
    }
    path.reverse();
    let path = order_messages_for_provider(
        path.into_iter()
            .filter(|message| message.metadata.get("placeholder") != Some(&Value::Bool(true)))
            .collect(),
    );
    let created = ConversationTimestamp::from_epoch_millis(now)?;
    let chat = ChatRecord {
        id: new_chat_id.to_owned(),
        title: source.title.map(|title| format!("Fork of {title}")),
        metadata: source.metadata,
        archived: false,
        created_at: created.clone(),
        updated_at: created,
    };
    state.chats.push(chat.clone());
    let mut copied = Vec::new();
    for mut original in path {
        if let Some(metadata) = original.metadata.as_object_mut() {
            metadata.remove("placeholder");
        }
        let message = append_message(
            state,
            MessageInput {
                chat_id: chat.id.clone(),
                run_id: None,
                role: original.role,
                content: original.content,
                tool_call_id: original.tool_call_id,
                tool_calls: original.tool_calls,
                model_result_json: original.model_result_json,
                metadata: original.metadata,
                ..Default::default()
            },
            now,
        )?;
        copied.push(message);
    }
    Ok((chat, copied))
}

/// `tasks::submit_turn`.
fn submit_turn(
    state: &mut State,
    input: SubmitTask,
    messages: Vec<MessageInput>,
) -> Result<TaskRecord> {
    if let Some(task) = state.task(&input.task_id) {
        if task.kind != input.kind
            || task.scope_id != input.scope_id
            || task.payload != input.payload
            || task.depends_on != input.depends_on
            || task.parent_task_id != input.parent_task_id
        {
            return Err(Fault::TaskInput {
                reason: TaskInputReason::ReplayIdentityMismatch,
                message: "task replay identity mismatch".into(),
            });
        }
        return Ok(task);
    }
    let now = input.now;
    let task = insert_task(state, input)?;
    for message in messages {
        if message.role == "user" && message.run_id.is_none() {
            if message.chat_id != task.scope_id {
                return Err(Fault::InvalidInput(
                    "message scope does not match task".into(),
                ));
            }
        } else {
            validate_projection(&task, &message)?;
        }
        append_message(state, message, now)?;
    }
    Ok(task)
}

/// `session_ops::queue_or_stage_turn`: the active-turn check and the
/// insertion commit together.
fn queue_or_stage(state: &mut State, input: QueueOrStage) -> Result<QueueAdmission> {
    let unrelated = state.tasks.iter().any(|task| {
        task.scope_id == input.session_id && task.kind != input.task_kind && live(task.status)
    });
    if unrelated {
        return Err(Fault::ScopeBusy(input.session_id));
    }
    let active: Option<String> = state
        .tasks
        .iter()
        .find(|task| task.scope_id == input.session_id && live(task.status))
        .map(|task| task.task_id.clone());
    let mut create = input.input;
    if let Some(active) = &active
        && input.target_the_live_turn
    {
        create.target_task_id = Some(active.clone());
    }
    let now_text = text(input.now)?;
    let (record, _) = insert_input(state, create, &now_text)?;
    let stageable = active.is_none()
        && record.status == AgentInputStatus::Pending
        && record.on_settled != Some(AgentInputSettlePolicy::Cancel);
    if !stageable {
        return Ok(QueueAdmission::Queued(record));
    }
    let Some(candidate) = input.candidate else {
        return Ok(QueueAdmission::Queued(record));
    };
    if convert_candidate(state, &input.session_id, &candidate, input.now)? {
        let task = state.required_task(&candidate.task.task_id)?;
        return Ok(QueueAdmission::Staged(task));
    }
    let record = state.input(&record.input_id).unwrap_or(record);
    Ok(QueueAdmission::Queued(record))
}

/// `session_ops::stage_queued_turn`.
fn stage_queued(state: &mut State, input: StageQueuedTurn) -> Result<Option<String>> {
    // The store's busy predicate: a merely-queued turn of the same kind is
    // NOT busy, unless it is parked for a retry.
    let busy = state.tasks.iter().any(|task| {
        task.scope_id == input.session_id
            && task.kind == input.task_kind
            && input.exclude_task_id.as_deref() != Some(task.task_id.as_str())
            && (matches!(
                task.status,
                TaskStatus::Running | TaskStatus::Interrupted | TaskStatus::WaitingApproval
            ) || (task.status == TaskStatus::Queued && task.retry_pending))
    });
    if busy
        || unrelated_live_task(
            state,
            &input.session_id,
            &input.task_kind,
            input.exclude_task_id.as_deref(),
        )
    {
        return Ok(None);
    }
    let Some(record) = state.input(&input.input_id) else {
        return Ok(None);
    };
    if record.status != AgentInputStatus::Pending
        || record.session_id != input.session_id
        || record.kind != input.expected_kind
    {
        return Ok(None);
    }
    let task_id = input.task.task_id.clone();
    let candidate = StagedCandidate {
        input_id: input.input_id,
        task: input.task,
        messages: input.messages,
    };
    if convert_candidate(state, &input.session_id, &candidate, input.now)? {
        Ok(Some(task_id))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Managed content and receipts.
// ---------------------------------------------------------------------------

/// `managed_content::resolve_run_content`. Templates are looked up verbatim
/// from the seeded catalog; argument substitution is the host's renderer and
/// no conformance case pins a template through this port.
fn resolve_run_content(
    state: &State,
    selection: &Value,
    authorized_connectors: &[String],
    tolerate_unavailable: bool,
) -> Result<Value> {
    let mut messages = Vec::new();
    if let Some(template) = selection.get("template").filter(|value| !value.is_null()) {
        let found = state
            .namespace(PROMPT_TEMPLATES_NAMESPACE)
            .into_iter()
            .map(|(_, value)| value)
            .find(|record| {
                record["id"] == template["id"] && record["revision"] == template["revision"]
            })
            .ok_or_else(|| Fault::NotFound("Prompt template revision not found.".into()))?;
        messages = found["messages"].as_array().cloned().unwrap_or_default();
    }
    let skills = match selection.get("skills") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(Fault::InvalidInput("Invalid skill selection.".into())),
    };
    let mut seen = std::collections::HashSet::new();
    if skills.len() > 20 {
        return Err(Fault::InvalidInput("Invalid skill selection.".into()));
    }
    for skill in &skills {
        let id = skill["id"].as_str().unwrap_or("").to_owned();
        if id.is_empty() || !seen.insert(id) {
            return Err(Fault::InvalidInput("Invalid skill selection.".into()));
        }
    }
    let mut blocks = Vec::new();
    let mut unavailable = Vec::new();
    let mut used_connectors = BTreeSet::new();
    for skill in &skills {
        let id = skill["id"].as_str().unwrap_or("");
        let revision = skill["revision"].as_str().unwrap_or("");
        let leg = SkillLeg {
            id,
            revision,
            authorized: authorized_connectors,
        };
        let failure = resolve_skill(state, &leg, &mut blocks, &mut used_connectors);
        if let Err(error) = failure {
            if !tolerate_unavailable {
                return Err(error);
            }
            unavailable.push(json!({ "id": id, "reason": error.to_string() }));
        }
    }
    Ok(json!({
        "blocks": blocks,
        "messages": messages,
        "unavailable": unavailable,
        "authorizedConnectors": used_connectors.into_iter().collect::<Vec<_>>(),
    }))
}

/// What one skill leg of `resolve_run_content` reads and writes.
struct SkillLeg<'a> {
    id: &'a str,
    revision: &'a str,
    authorized: &'a [String],
}

/// One skill's leg of `resolve_run_content`; `Err` is what gets reported or
/// tolerated. The content-hash re-check the engine runs is omitted: the
/// in-memory catalog is written by this process and cannot race itself.
fn resolve_skill(
    state: &State,
    leg: &SkillLeg<'_>,
    blocks: &mut Vec<Value>,
    used_connectors: &mut BTreeSet<String>,
) -> Result<()> {
    let SkillLeg {
        id,
        revision,
        authorized: authorized_connectors,
    } = *leg;
    let record = state.skill(id).ok_or_else(|| {
        Fault::InvalidInput("Selected skill revision changed or disappeared.".into())
    })?;
    if record["revision"].as_str() != Some(revision) {
        return Err(Fault::InvalidInput(
            "Selected skill revision changed or disappeared.".into(),
        ));
    }
    let requirements = record["connectorRequirements"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if requirements
        .iter()
        .any(|need| !authorized_connectors.iter().any(|grant| grant == need))
    {
        return Err(Fault::SelectedConnectorRequired(
            "Skill requires a selected authorized connector.".into(),
        ));
    }
    used_connectors.extend(requirements);
    blocks.push(json!({
        "id": format!("skill:{}@{}", record["id"].as_str().unwrap_or(""), revision),
        "title": format!("Skill: {}", record["name"].as_str().unwrap_or("")),
        "content": record["instructions"],
        "priority": 100,
    }));
    Ok(())
}

/// `execution_resolution::validate`.
fn validate_resolution(request: &ResolveExecution) -> Result<()> {
    let invalid = |message: &str| Err(Fault::InvalidInput(message.to_owned()));
    match (request.outcome, &request.result) {
        (ResolutionOutcome::Completed, Some(result)) if result.is_object() => {
            if result.to_string().len() > 262_144 {
                return invalid("result exceeds 262144 bytes");
            }
        }
        (ResolutionOutcome::Completed, _) => {
            return invalid("a completed resolution requires a tool-result object");
        }
        (_, Some(_)) => return invalid("only a completed resolution may carry a result"),
        (_, None) => {}
    }
    if request
        .note
        .as_ref()
        .is_some_and(|note| note.chars().count() > 2000)
    {
        return invalid("note exceeds 2000 characters");
    }
    if request
        .resolved_by
        .as_ref()
        .is_some_and(|actor| actor.is_empty() || actor.chars().count() > 200)
    {
        return invalid("resolvedBy must be 1-200 characters when present");
    }
    Ok(())
}

/// `execution_resolution::resolve_execution`.
fn resolve_execution(
    state: &mut State,
    execution_id: &str,
    request: ResolveExecution,
) -> Result<ExecutionReceiptRecord> {
    validate_resolution(&request)?;
    let Some(record) = state
        .receipts
        .iter()
        .find(|receipt| receipt.execution_id == execution_id)
        .cloned()
    else {
        return Err(Fault::NotFound(format!("execution receipt {execution_id}")));
    };
    if record.status != ExecutionReceiptStatus::Admitted {
        return match &record.resolution {
            Some(existing)
                if existing.outcome == request.outcome
                    && (request.outcome != ResolutionOutcome::Completed
                        || record.result == request.result) =>
            {
                Ok(record)
            }
            _ => Err(Fault::ReceiptTransition {
                reason: ReceiptRefusal::NotUncertain,
                message: format!(
                    "{RECEIPT_NOT_UNCERTAIN}: execution {execution_id} is already finished"
                ),
            }),
        };
    }
    let active = state
        .task(&record.run_id)
        .is_some_and(|task| !task.status.is_terminal());
    if active {
        return Err(Fault::ReceiptTransition {
            reason: ReceiptRefusal::RunActive,
            message: format!(
                "{RECEIPT_RUN_ACTIVE}: run {} is still active",
                record.run_id
            ),
        });
    }
    let mut record = record;
    record.status = match request.outcome {
        ResolutionOutcome::Completed => ExecutionReceiptStatus::Completed,
        _ => ExecutionReceiptStatus::Resolved,
    };
    record.result = request.result.clone();
    record.resolution = Some(ExecutionResolution {
        outcome: request.outcome,
        note: request.note.clone(),
        resolved_at: request.now,
        resolved_by: request.resolved_by.clone(),
    });
    let slot = state
        .receipts
        .iter_mut()
        .find(|receipt| receipt.execution_id == execution_id)
        .expect("present");
    *slot = record.clone();
    Ok(record)
}

// ---------------------------------------------------------------------------
// The conformance fixture: the facts and states the port cannot write.
// ---------------------------------------------------------------------------

/// The in-memory twin's own handle: the repository under test is itself, so
/// the fixture's seeded writes go through the same one lock.
#[derive(Clone, Default)]
pub struct InMemorySessionFixture {
    repo: InMemorySessionRepository,
}

impl InMemorySessionFixture {
    /// A fixture over a fresh repository.
    pub fn new() -> Self {
        Self::default()
    }
}

/// The fault a seeded write can raise: only the state-level refusals.
#[derive(Debug, thiserror::Error)]
#[error("fixture write refused: {0}")]
pub struct InMemoryFixtureError(String);

/// The catalog shape gates the store applies when seeding.
fn seeded_provider(value: &Value) -> Result<()> {
    if !is_provider(value) {
        return Err(Fault::InvalidInput("Malformed provider".into()));
    }
    Ok(())
}

fn seeded_capabilities(value: &Value) -> Result<()> {
    if !is_capabilities(value) {
        return Err(Fault::InvalidInput(
            "Malformed provider capabilities".into(),
        ));
    }
    Ok(())
}

fn seeded_models(models: &[Value]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    if models.iter().any(|item| {
        !is_model(item) || !seen.insert(item["modelId"].as_str().unwrap_or_default().to_owned())
    }) {
        return Err(Fault::InvalidInput(
            "Malformed or duplicate provider model".into(),
        ));
    }
    Ok(())
}

/// `managed_content::skill_content_hash`: the content-addressed revision.
fn content_hash(text: &str) -> String {
    agentkit_kernel::sha256_hex(text)
}

/// `managed_content::connector_requirements`: the frontmatter
/// `metadata.requires-connectors` list.
fn connector_requirements(frontmatter: &str) -> Vec<String> {
    let Some(line) = frontmatter
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("requires-connectors:"))
    else {
        return vec![];
    };
    line["requires-connectors:".len()..]
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `managed_content::parse_skill_document`, reduced to the fields the store
/// stores: the frontmatter's `name` / `description` and the body.
fn parse_skill(text: &str) -> Result<(String, String, String)> {
    let invalid = |reason: &str| Fault::InvalidInput(reason.to_owned());
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines.first() != Some(&"---") {
        return Err(invalid("SKILL.md requires YAML frontmatter."));
    }
    let Some(closing) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| **line == "---")
        .map(|(index, _)| index)
    else {
        return Err(invalid("SKILL.md requires YAML frontmatter."));
    };
    let field = |key: &str| -> String {
        lines[1..closing]
            .iter()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim() == key)
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default()
    };
    Ok((
        field("name"),
        field("description"),
        lines[closing + 1..].join("\n"),
    ))
}

#[async_trait]
impl crate::conformance::session_repository::SessionFixture for InMemorySessionFixture {
    type Repo = InMemorySessionRepository;
    type Error = InMemoryFixtureError;

    fn repo(&self) -> &Self::Repo {
        &self.repo
    }

    async fn put_provider(&self, provider: Value, capabilities: Value) -> Result<(), Self::Error> {
        let id = provider["id"].as_str().unwrap_or_default().to_owned();
        self.repo
            .atomic(move |state| {
                seeded_provider(&provider)?;
                seeded_capabilities(&capabilities)?;
                state.settings.push(Setting {
                    namespace: PROVIDERS_NAMESPACE.into(),
                    key: id.clone(),
                    value: provider,
                });
                state.settings.push(Setting {
                    namespace: PROVIDER_CAPABILITIES_NAMESPACE.into(),
                    key: id,
                    value: capabilities,
                });
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    async fn delete_provider(&self, id: &str) -> Result<(), Self::Error> {
        let id = id.to_owned();
        self.repo
            .atomic(move |state| {
                state.settings.retain(|row| {
                    !(matches!(
                        row.namespace.as_str(),
                        PROVIDERS_NAMESPACE
                            | PROVIDER_CAPABILITIES_NAMESPACE
                            | PROVIDER_MODELS_NAMESPACE
                    ) && row.key == id)
                });
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    async fn put_provider_models(&self, id: &str, models: Vec<Value>) -> Result<(), Self::Error> {
        let id = id.to_owned();
        self.repo
            .atomic(move |state| {
                seeded_models(&models)?;
                state
                    .settings
                    .retain(|row| !(row.namespace == PROVIDER_MODELS_NAMESPACE && row.key == id));
                state.settings.push(Setting {
                    namespace: PROVIDER_MODELS_NAMESPACE.into(),
                    key: id.clone(),
                    value: Value::Array(models),
                });
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    async fn put_skill(
        &self,
        source: &str,
        path: &str,
        content: &str,
    ) -> Result<Value, Self::Error> {
        let (source, path, content) = (source.to_owned(), path.to_owned(), content.to_owned());
        self.repo
            .atomic(move |state| {
                if source.is_empty() || source.contains(':') {
                    return Err(Fault::InvalidInput(
                        "Skill source must be a non-empty string without ':'".into(),
                    ));
                }
                let (name, description, instructions) = parse_skill(&content)?;
                let record = json!({
                    "id": format!("{source}:{path}"),
                    "revision": content_hash(&content),
                    "sourceId": source,
                    "path": path,
                    "name": name,
                    "description": description,
                    "connectorRequirements": connector_requirements(&content),
                    "instructions": instructions,
                    "content": content,
                });
                state.skills.retain(|skill| {
                    !(skill.record["sourceId"] == record["sourceId"]
                        && skill.record["path"] == record["path"])
                });
                state.skills.push(Skill {
                    record: record.clone(),
                });
                Ok(skill_entry(&record))
            })
            .await
            .map_err(fixture_fault)
    }

    async fn put_setting(
        &self,
        namespace: &str,
        key: &str,
        value: Value,
    ) -> Result<(), Self::Error> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        if namespace == CONTEXT_SUMMARIES_NAMESPACE
            && (!is_summary(&value) || value["summaryId"].as_str() != Some(key.as_str()))
        {
            return Err(InMemoryFixtureError("Malformed context summary".into()));
        }
        self.repo
            .atomic(move |state| {
                state
                    .settings
                    .retain(|row| !(row.namespace == namespace && row.key == key));
                state.settings.push(Setting {
                    namespace,
                    key,
                    value,
                });
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    async fn put_agent(
        &self,
        agent_id: &str,
        definition: agentkit_sessions::AgentDefinition,
        now: i64,
    ) -> Result<(), Self::Error> {
        let agent_id = agent_id.to_owned();
        self.repo
            .atomic(move |state| {
                let record = agentkit_sessions::AgentRecord {
                    agent_id: agent_id.clone(),
                    scope: "team".into(),
                    owner_id: "owner".into(),
                    lifecycle: agentkit_sessions::AgentLifecycle::Active,
                    current_revision: 1,
                    definition,
                    created_at: now,
                    updated_at: now,
                };
                state.agents.retain(|agent| agent.agent_id != agent_id);
                state.agents.push(record);
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    /// `session_ops::park_turn`: RUNNING -> WAITING_APPROVAL and the request
    /// row, in one lock.
    async fn park_turn(
        &self,
        task_id: &str,
        request: CreateAgentInput,
        now: i64,
    ) -> Result<String, Self::Error> {
        let task_id = task_id.to_owned();
        self.repo
            .atomic(move |state| {
                let mut task = state.required_task(&task_id)?;
                task.status = TaskStatus::WaitingApproval;
                state.save_task(&task);
                let now_text = text(now)?;
                let (record, _) = insert_input(state, request, &now_text)?;
                Ok(record.input_id)
            })
            .await
            .map_err(fixture_fault)
    }

    /// `tasks::append_events`, without the fence: the fixture seeds the log
    /// a reader is being tested against.
    async fn put_events(
        &self,
        task_id: &str,
        events: Vec<Value>,
        now: i64,
    ) -> Result<(), Self::Error> {
        let task_id = task_id.to_owned();
        self.repo
            .atomic(move |state| {
                state.required_task(&task_id)?;
                for event in events {
                    let seq = event["seq"].as_f64().unwrap_or_default();
                    state.events.push(EventRecord {
                        task_id: task_id.clone(),
                        seq,
                        event,
                        created_at: now,
                    });
                }
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    /// `execution_receipts::admit_execution` plus a crash-like settle.
    async fn admit_receipt(
        &self,
        receipt: ExecutionReceiptRecord,
        settle_run: bool,
        now: i64,
    ) -> Result<(), Self::Error> {
        self.repo
            .atomic(move |state| {
                let mut run = state.required_task(&receipt.run_id)?;
                state.receipts.push(receipt);
                if settle_run {
                    run.status = TaskStatus::Failed;
                    run.finished_at = Some(now);
                    state.save_task(&run);
                }
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }

    /// `teammates::admit_teammate_task` with a pinned profile: the task lands
    /// bound to `agent_id` with the sealed document.
    async fn put_pinned_turn(
        &self,
        task_id: &str,
        agent_id: &str,
        profile: Value,
        now: i64,
    ) -> Result<(), Self::Error> {
        let (task_id, agent_id) = (task_id.to_owned(), agent_id.to_owned());
        self.repo
            .atomic(move |state| {
                // An admission creates the row it binds; the suite names the
                // id, so a task that is not there yet lands as a fresh
                // queued turn of the agent's own scope.
                if state.task(&task_id).is_none() {
                    insert_task(
                        state,
                        agentkit_engine::SubmitTask {
                            task_id: task_id.clone(),
                            kind: "teammate.turn".into(),
                            scope_id: format!("team:{agent_id}"),
                            payload: json!({"key": task_id}),
                            priority: 0,
                            now,
                            available_at: None,
                            depends_on: vec![],
                            parent_task_id: None,
                            exclusive_scope: false,
                        },
                    )?;
                }
                let mut task = state.required_task(&task_id)?;
                task.agent_id = Some(agent_id);
                task.agent_revision = Some(1);
                task.run_profile = Some(Box::new(agentkit_engine::PinnedRunProfile {
                    digest: run_profile_digest(&profile),
                    profile,
                    pinned_at: now,
                }));
                state.save_task(&task);
                Ok(())
            })
            .await
            .map_err(fixture_fault)
    }
}

fn fixture_fault(error: Fault) -> InMemoryFixtureError {
    InMemoryFixtureError(error.to_string())
}
