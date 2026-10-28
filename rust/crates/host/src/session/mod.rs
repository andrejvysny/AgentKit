//! The durable agent-session service (ADR 0016-0019): the single owner of
//! session state, steering, follow-ups, human requests, plan approvals,
//! interruption and queueing. Port of `sessions/agent-session-service.ts`.
//!
//! The store is the only authority: every busy check and every queue
//! conversion is a single actor job, so there is no check-then-write window
//! for a concurrent submit to lose a race in. The service holds no provider
//! client; `compact` is deliberately DEFERRED (it records the request; the
//! session's next turn performs it).

mod descriptor;
mod report;

pub use descriptor::{
    CHAT_TURN_TASK_KIND, REASONING_EFFORTS, is_valid_session_turn_kind, read_session_descriptor,
    session_content_selection, session_turn_kind,
};
pub use report::{FALLBACK_CONTEXT_WINDOW_TOKENS, list_turn_tasks, session_usage, summarize_turn};

use crate::{HostResult, SharedRuntime};
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputSettlePolicy,
    AgentInputStatus, AnswerHumanRequest, AnswerReceipt, COMPACT_REQUESTED_METADATA_KEY,
    ChatRecord, CreateAgentInput, MessageInput, SESSION_METADATA_KEY,
    STANDING_APPROVALS_METADATA_KEY, SqliteStore, StageQueuedTurn, SubmitTask, TaskRecord,
    TaskStatus, execution_state_for_mode, plan_request_of, read_execution_state, standing_grants,
    with_execution,
};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// A session-domain failure with the TypeScript error's code (`chat_busy`,
/// `session_not_found`, `invalid_input`, `answer_conflict`, ...).
#[derive(Clone, Debug, PartialEq)]
pub struct SessionError {
    pub code: String,
    pub message: String,
}

impl SessionError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
    fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }
    fn chat_busy(session_id: &str) -> Self {
        Self::new("chat_busy", format!("Chat {session_id} is busy."))
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SessionError {}

impl From<agentkit_store::StoreError> for SessionError {
    fn from(error: agentkit_store::StoreError) -> Self {
        use agentkit_store::StoreError as E;
        match error {
            E::NotFound(message) => Self::new("not_found", message),
            E::Duplicate(message) => {
                let code = if message.starts_with("answer_conflict") {
                    "answer_conflict"
                } else if message.starts_with("input_conflict") {
                    "input_conflict"
                } else {
                    "duplicate"
                };
                Self::new(code, message)
            }
            E::ScopeBusy(scope) => Self::chat_busy(&scope),
            E::InvalidTransition(message) => Self::new("invalid_state", message),
            E::LeaseLost(task_id) => Self::new("invalid_state", format!("lease lost: {task_id}")),
            E::InvalidInput(message) => Self::new("invalid_input", message),
            other => Self::new("store_failure", other.to_string()),
        }
    }
}

pub type SessionResult<T> = Result<T, SessionError>;

// ---------------------------------------------------------------------------
// Plan-approval policy (ADR 0019 part 2) — the HOST decides escalation.
// ---------------------------------------------------------------------------

/// What the human's approval is being asked about (context for the veto).
pub struct PlanGrantIntent<'a> {
    pub session_id: &'a str,
    pub turn_id: &'a str,
    pub request_id: &'a str,
    pub plan_digest: &'a str,
    pub requested_mode: Option<&'a str>,
}

pub enum PlanAuthorization {
    Allowed,
    /// The denial keeps the session in plan mode; the reason is recorded in
    /// the forced `{decision:"deny", note:"plan_not_authorized: <reason>"}`.
    Denied(Option<String>),
}

/// How a host turns an approvable plan refusal into a grant. The model may
/// name a requested mode; the policy decides what it gets.
#[async_trait]
pub trait PlanApprovalPolicy: Send + Sync {
    /// Tool names whose approvable refusal is a PLAN approval rather than a
    /// per-call one — `["submit_plan"]` for the coding pack.
    fn tool_names(&self) -> Vec<String>;
    /// The mode the session moves to when the user approves. Resolving to
    /// `plan` or the empty string denies.
    async fn granted_mode(&self, session_id: &str) -> HostResult<String>;
    /// Extra veto (role ceiling, untrusted workspace).
    async fn authorize(&self, _intent: &PlanGrantIntent<'_>) -> HostResult<PlanAuthorization> {
        Ok(PlanAuthorization::Allowed)
    }
}

// ---------------------------------------------------------------------------
// Inputs and results
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct CreateSessionInput {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub profile_id: String,
    pub profile_digest: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    /// `{root, trusted}`; validated by the descriptor reader.
    pub workspace: Option<Value>,
    /// `SessionContextOptions`; validated by the descriptor reader.
    pub context: Option<Value>,
    pub skills: Option<Vec<Value>>,
    pub turn_kind: Option<String>,
    pub execution_mode: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhenBusy {
    Reject,
    Queue,
    Steer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnSettled {
    FollowUp,
    Cancel,
}

impl From<OnSettled> for AgentInputSettlePolicy {
    fn from(value: OnSettled) -> Self {
        match value {
            OnSettled::FollowUp => Self::FollowUp,
            OnSettled::Cancel => Self::Cancel,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SendInput {
    pub content: Value,
    pub input_id: Option<String>,
    pub when_busy: WhenBusy,
    pub turn_id: Option<String>,
    pub on_settled: Option<OnSettled>,
}

#[derive(Clone, Debug)]
pub struct FollowUpInput {
    pub content: Value,
    pub input_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SteerInput {
    pub content: Value,
    pub input_id: Option<String>,
    pub turn_id: Option<String>,
    pub on_settled: Option<OnSettled>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentSendResult {
    Turn {
        turn_id: String,
        user_message_id: String,
        assistant_message_id: String,
    },
    Queued {
        input: Box<AgentInputRecord>,
    },
}

#[derive(Clone, Debug)]
pub struct AnswerInput {
    pub request_id: String,
    pub answer_id: String,
    pub response: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentSessionState {
    pub session_id: String,
    pub descriptor: Value,
    pub active_turn: Option<Value>,
    pub pending: Vec<AgentInputRecord>,
    pub waiting: Option<AgentInputRecord>,
    pub waiting_approval: Option<Value>,
    pub waiting_plan: Option<Value>,
    pub execution: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueueKind {
    FollowUp,
    Steer,
}

impl QueueKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::FollowUp => "follow_up",
            Self::Steer => "steer",
        }
    }
    fn input_kind(self) -> AgentInputKind {
        match self {
            Self::FollowUp => AgentInputKind::FollowUp,
            Self::Steer => AgentInputKind::Steer,
        }
    }
}

/// The re-checks of the idle/busy race before giving up with `chat_busy`.
const MAX_SUBMIT_ATTEMPTS: u32 = 3;

pub struct AgentSessionServiceDeps {
    pub store: SqliteStore,
    pub runtime: SharedRuntime,
    /// Plan-mode escalation policy. Absent, plan approvals are never honored
    /// and a session in plan mode stays read-only.
    pub plan_approval: Option<Arc<dyn PlanApprovalPolicy>>,
    /// Best-effort queue poke (the TS `taskRunner.enqueue`); polling still
    /// covers every miss, so `None` only costs latency.
    pub wake: Option<crate::WakeHandle>,
}

struct Deps {
    store: SqliteStore,
    runtime: SharedRuntime,
    plan_approval: Option<Arc<dyn PlanApprovalPolicy>>,
    wake: Option<crate::WakeHandle>,
}

/// The session service. Cheap to clone; all state lives in the store.
#[derive(Clone)]
pub struct AgentSessionService {
    deps: Arc<Deps>,
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Queueing intent for one input: its stable identity and what should
/// happen to it once the busy turn it queued behind settles.
#[derive(Clone, Default)]
struct QueueIntent {
    input_id: Option<String>,
    turn_id: Option<String>,
    on_settled: Option<OnSettled>,
}

fn turn_id_for(input_id: &str) -> String {
    format!("turn:{input_id}")
}

fn read_input_content(content: &Value) -> Option<Value> {
    let value = content.get("content")?;
    (value.is_string() || value.is_array()).then(|| value.clone())
}

impl AgentSessionService {
    pub fn new(deps: AgentSessionServiceDeps) -> Self {
        Self {
            deps: Arc::new(Deps {
                store: deps.store,
                runtime: deps.runtime,
                plan_approval: deps.plan_approval,
                wake: deps.wake,
            }),
        }
    }

    fn store(&self) -> &SqliteStore {
        &self.deps.store
    }
    fn now_ms(&self) -> i64 {
        self.deps.runtime.now_ms()
    }
    fn mint(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.deps.runtime.id())
    }
    fn now_iso(&self) -> String {
        agentkit_store::ConversationTimestamp::from_epoch_millis(self.now_ms())
            .expect("host clock must fit the JavaScript Date range")
            .wire_text()
            .expect("validated clock")
            .into_owned()
    }
    fn wake(&self) {
        if let Some(wake) = &self.deps.wake {
            wake.wake();
        }
    }

    // -- session lifecycle --------------------------------------------------

    pub async fn create_session(
        &self,
        input: &CreateSessionInput,
    ) -> SessionResult<AgentSessionState> {
        let mut metadata = match input.metadata.clone().unwrap_or_else(|| json!({})) {
            Value::Object(map) => map,
            _ => {
                return Err(SessionError::invalid_input("metadata must be an object."));
            }
        };
        for key in [SESSION_METADATA_KEY, STANDING_APPROVALS_METADATA_KEY] {
            if metadata.contains_key(key) {
                // Grants are written only by an answered approval; a
                // caller-supplied one would be a standing approval no human gave.
                return Err(SessionError::invalid_input(format!(
                    "metadata must not contain {key}."
                )));
            }
        }
        if let Some(kind) = &input.turn_kind
            && !is_valid_session_turn_kind(kind)
        {
            return Err(SessionError::invalid_input(
                "turnKind must be a dotted lowercase task kind outside the reserved chat.* / agentkit.* prefixes.",
            ));
        }
        let mut raw = Map::new();
        raw.insert("version".into(), Value::from(1));
        let mut profile = Map::new();
        profile.insert("id".into(), input.profile_id.clone().into());
        if let Some(digest) = &input.profile_digest {
            profile.insert("digest".into(), digest.clone().into());
        }
        raw.insert("profile".into(), Value::Object(profile));
        for (key, value) in [("providerId", &input.provider_id), ("model", &input.model)] {
            if let Some(value) = value {
                raw.insert(key.into(), value.clone().into());
            }
        }
        raw.insert("createdAt".into(), self.now_iso().into());
        for (key, value) in [
            ("workspace", &input.workspace),
            ("context", &input.context),
            ("skills", &input.skills.clone().map(Value::Array)),
            ("turnKind", &input.turn_kind.clone().map(Value::String)),
        ] {
            if let Some(value) = value {
                raw.insert(key.into(), value.clone());
            }
        }
        metadata.insert(SESSION_METADATA_KEY.into(), Value::Object(raw));
        if read_session_descriptor(&Value::Object(metadata.clone())).is_none() {
            return Err(SessionError::invalid_input(
                "profile.id must be a non-empty string.",
            ));
        }
        let state = match &input.execution_mode {
            Some(mode) => {
                // Validate the mode first so a bad one cannot half-create
                // the session.
                let mode = mode.clone();
                let updated_at = self.now_iso();
                Some(
                    execution_state_for_mode(&mode, &updated_at, None)
                        .ok_or_else(|| SessionError::invalid_input("mode must be non-empty."))?,
                )
            }
            None => None,
        };
        let metadata = with_execution(&Value::Object(metadata), state);
        let session_id = input
            .session_id
            .clone()
            .unwrap_or_else(|| self.mint("chat_"));
        let chat = self
            .store()
            .create_chat(&session_id, input.title.clone(), metadata, self.now_ms())
            .await?;
        self.get_state(&chat.id).await
    }

    pub async fn get_state(&self, session_id: &str) -> SessionResult<AgentSessionState> {
        let (chat, descriptor) = self.require_session(session_id).await?;
        let active = self
            .active_turn(session_id, &session_turn_kind(&descriptor))
            .await?;
        let records = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: None,
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        let waiting = records
            .iter()
            .find(|record| record.kind == AgentInputKind::HumanRequest)
            .cloned();
        let pending: Vec<AgentInputRecord> = records
            .into_iter()
            .filter(|record| record.kind != AgentInputKind::HumanRequest)
            .collect();
        let execution = read_execution_state(&chat.metadata);
        let waiting_plan = waiting.as_ref().and_then(|waiting| {
            self.plan_request_of(waiting).map(|plan| {
                let mut out = plan.as_object().cloned().unwrap_or_default();
                out.insert("requestId".into(), waiting.input_id.clone().into());
                out.insert(
                    "toolName".into(),
                    waiting.content["toolName"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                );
                Value::Object(out)
            })
        });
        Ok(AgentSessionState {
            session_id: session_id.to_owned(),
            descriptor,
            active_turn: active.map(|task| {
                json!({"turnId": task.task_id, "status": serde_json::to_value(task.status).unwrap_or(Value::Null)})
            }),
            pending,
            waiting: waiting.clone(),
            waiting_approval: waiting.as_ref().and_then(approval_of),
            waiting_plan,
            execution,
        })
    }

    /// The session's durable permission state, when one was ever set.
    pub async fn get_execution(&self, session_id: &str) -> SessionResult<Option<Value>> {
        let (chat, _) = self.require_session(session_id).await?;
        Ok(read_execution_state(&chat.metadata))
    }

    /// An explicit, persisted mode switch by the HOST or the user — never by
    /// the model. Entering `plan` drops any grant, so a session that was
    /// cleared to edit cannot read its way back into write access without a
    /// new approval.
    pub async fn set_execution_mode(&self, session_id: &str, mode: &str) -> SessionResult<Value> {
        let now = self.now_ms();
        let mode = mode.to_owned();
        let session = session_id.to_owned();
        Ok(self
            .store()
            .mutate_chat(session_id, now, move |chat| {
                if read_session_descriptor(&chat.metadata).is_none() {
                    return Err(agentkit_store::StoreError::NotFound(format!(
                        "Session {session} is not an agent session."
                    )));
                }
                let updated_at = agentkit_store::ConversationTimestamp::from_epoch_millis(now)?
                    .wire_text()?
                    .into_owned();
                let state = execution_state_for_mode(
                    &mode,
                    &updated_at,
                    read_execution_state(&chat.metadata).as_ref(),
                )
                .ok_or_else(|| {
                    agentkit_store::StoreError::InvalidInput("mode must be non-empty.".to_owned())
                })?;
                chat.metadata = with_execution(&chat.metadata, Some(state.clone()));
                Ok(state)
            })
            .await?)
    }

    // -- sending -------------------------------------------------------------

    pub async fn send(
        &self,
        session_id: &str,
        input: &SendInput,
    ) -> SessionResult<AgentSendResult> {
        if input.when_busy != WhenBusy::Reject && input.input_id.is_none() {
            return Err(SessionError::invalid_input(format!(
                "inputId is required when whenBusy is \"{}\".",
                match input.when_busy {
                    WhenBusy::Reject => "reject",
                    WhenBusy::Queue => "queue",
                    WhenBusy::Steer => "steer",
                }
            )));
        }
        let (_, descriptor) = self.require_session(session_id).await?;
        if input.when_busy == WhenBusy::Reject {
            return self
                .submit_turn(
                    session_id,
                    &descriptor,
                    &input.content,
                    input.input_id.as_deref(),
                )
                .await;
        }
        let kind = if input.when_busy == WhenBusy::Steer {
            QueueKind::Steer
        } else {
            QueueKind::FollowUp
        };
        self.submit_or_queue(
            session_id,
            &descriptor,
            kind,
            input.content.clone(),
            QueueIntent {
                input_id: input.input_id.clone(),
                turn_id: input.turn_id.clone(),
                on_settled: input.on_settled,
            },
        )
        .await
    }

    pub async fn follow_up(
        &self,
        session_id: &str,
        input: &FollowUpInput,
    ) -> SessionResult<AgentSendResult> {
        let (_, descriptor) = self.require_session(session_id).await?;
        self.submit_or_queue(
            session_id,
            &descriptor,
            QueueKind::FollowUp,
            input.content.clone(),
            QueueIntent {
                input_id: input.input_id.clone(),
                turn_id: None,
                on_settled: None,
            },
        )
        .await
    }

    /// On an idle session a steer is just a new turn.
    pub async fn steer(
        &self,
        session_id: &str,
        input: &SteerInput,
    ) -> SessionResult<AgentSendResult> {
        let (_, descriptor) = self.require_session(session_id).await?;
        self.submit_or_queue(
            session_id,
            &descriptor,
            QueueKind::Steer,
            input.content.clone(),
            QueueIntent {
                input_id: input.input_id.clone(),
                turn_id: input.turn_id.clone(),
                on_settled: input.on_settled,
            },
        )
        .await
    }

    /// Convert the oldest queued input into a turn. Refused with `chat_busy`
    /// while the session has unfinished work; `None` when nothing is queued.
    pub async fn flush_queue(&self, session_id: &str) -> SessionResult<Option<AgentSendResult>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        let turn_kind = session_turn_kind(&descriptor);
        let pending = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::FollowUp, AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        if pending.is_empty() {
            return Ok(None);
        }
        // Keeps a refused flush from cancelling steers meant for the live
        // turn; the staged conversion's busy check stays the authoritative
        // guard.
        if self.active_turn(session_id, &turn_kind).await?.is_some() {
            return Err(SessionError::chat_busy(session_id));
        }
        for candidate in self
            .queue_candidates(session_id, &descriptor, &turn_kind)
            .await?
        {
            let input_id = candidate.input_id.clone();
            let user_message_id = candidate.messages[0].id.clone().unwrap_or_default();
            let assistant_message_id = candidate.messages[1].id.clone().unwrap_or_default();
            if self.store().stage_queued_turn(candidate).await?.is_some() {
                self.wake();
                return Ok(Some(AgentSendResult::Turn {
                    turn_id: turn_id_for(&input_id),
                    user_message_id,
                    assistant_message_id,
                }));
            }
            // Lost the race for this input (a concurrent flush settled it);
            // keep scanning.
        }
        Ok(None)
    }

    /// Every queued input that may take the scope, in arrival order, with
    /// its task and messages fully staged. `on_settled: cancel` steers whose
    /// target has settled are cancelled here, never converted.
    async fn queue_candidates(
        &self,
        session_id: &str,
        descriptor: &Value,
        turn_kind: &str,
    ) -> SessionResult<Vec<StageQueuedTurn>> {
        let pending = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::FollowUp, AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        let mut out = Vec::new();
        for record in &pending {
            if record.kind == AgentInputKind::Steer {
                if let Some(target) = &record.target_task_id {
                    let target = self.store().get_task(target).await?;
                    if target
                        .as_ref()
                        .is_some_and(|task| !task.status.is_terminal())
                    {
                        continue; // aimed at a live turn: not this flush's candidate
                    }
                }
                if record.on_settled == Some(AgentInputSettlePolicy::Cancel) {
                    let _ = self
                        .store()
                        .transition_input(
                            record.input_id.clone(),
                            vec![AgentInputStatus::Pending],
                            AgentInputStatus::Cancelled,
                            AgentInputPatch::default(),
                            self.now_ms(),
                        )
                        .await;
                    continue;
                }
            }
            let Some(content) = read_input_content(&record.content) else {
                continue; // malformed content is skipped, like the TS logger path
            };
            let user_message_id = self.mint("msg_");
            let assistant_message_id = self.mint("msg_");
            let task = self.stage_task(
                session_id,
                descriptor,
                &record.input_id,
                &user_message_id,
                &assistant_message_id,
            );
            out.push(StageQueuedTurn {
                session_id: session_id.to_owned(),
                task_kind: turn_kind.to_owned(),
                exclude_task_id: None,
                input_id: record.input_id.clone(),
                expected_kind: record.kind,
                task,
                messages: self.stage_messages(
                    session_id,
                    &turn_id_for(&record.input_id),
                    &content,
                    &user_message_id,
                    &assistant_message_id,
                    None,
                ),
                now: self.now_ms(),
            });
        }
        Ok(out)
    }

    // -- turn lifecycle -------------------------------------------------------

    pub async fn cancel(&self, session_id: &str, turn_id: &str) -> SessionResult<()> {
        let task = self.require_turn(session_id, turn_id).await?;
        let was_parked = task.status == TaskStatus::WaitingApproval;
        self.store().cancel_task(turn_id, self.now_ms()).await?;
        if was_parked {
            self.cancel_requests(session_id, turn_id).await?;
        }
        Ok(())
    }

    /// Stop a turn RESUMABLY (distinct from [`Self::cancel`]): the turn parks
    /// `interrupted` with its id, events and records intact, and
    /// [`Self::resume`] continues it with a new attempt.
    pub async fn interrupt(&self, session_id: &str, turn_id: &str) -> SessionResult<()> {
        self.require_turn(session_id, turn_id).await?;
        self.store()
            .request_interrupt(turn_id.to_owned(), self.now_ms())
            .await?;
        self.wake();
        Ok(())
    }

    /// Continue an interrupted turn; any other status is `invalid_state`.
    pub async fn resume(&self, session_id: &str, turn_id: &str) -> SessionResult<()> {
        let task = self.require_turn(session_id, turn_id).await?;
        if task.status != TaskStatus::Interrupted {
            return Err(SessionError::new(
                "invalid_state",
                format!(
                    "Turn {turn_id} is {:?}; only an interrupted turn can resume.",
                    task.status
                ),
            ));
        }
        self.store().resume_task(turn_id, self.now_ms()).await?;
        self.wake();
        Ok(())
    }

    // -- reports ---------------------------------------------------------------

    pub async fn list_turns(&self, session_id: &str) -> SessionResult<Vec<Value>> {
        let (_, descriptor) = self.require_session(session_id).await?;
        Ok(
            list_turn_tasks(self.store(), session_id, &session_turn_kind(&descriptor))
                .await?
                .iter()
                .map(summarize_turn)
                .collect(),
        )
    }

    pub async fn get_usage(&self, session_id: &str) -> SessionResult<Value> {
        let (_, descriptor) = self.require_session(session_id).await?;
        session_usage(self.store(), session_id, &descriptor).await
    }

    /// A JSON-serializable history: active-path messages, turns, inputs.
    pub async fn export_session(&self, session_id: &str) -> SessionResult<Value> {
        let (chat, descriptor) = self.require_session(session_id).await?;
        report::export_session(
            self.store(),
            session_id,
            &descriptor,
            &chat.metadata,
            &self.now_iso(),
        )
        .await
    }

    // -- human requests ---------------------------------------------------------

    /// Record the answer and re-admit the parked turn, then poke the queue.
    pub async fn answer(&self, input: &AnswerInput) -> SessionResult<AnswerReceipt> {
        let request = self
            .store()
            .get_input(input.request_id.clone())
            .await?
            .filter(|record| record.kind == AgentInputKind::HumanRequest)
            .ok_or_else(|| {
                SessionError::new(
                    "not_found",
                    format!("Human request not found: {}", input.request_id),
                )
            })?;
        self.require_session(&request.session_id).await?;
        let response_hash = sha256_hex(
            &serde_json::to_string(&input.response)
                .map_err(|error| SessionError::invalid_input(error.to_string()))?,
        );
        let mut plan_tool_names = vec![];
        let mut policy_denial = None;
        let mut granted_mode = None;
        if let Some(policy) = &self.deps.plan_approval {
            plan_tool_names = policy.tool_names();
            if let Some(plan) = plan_request_of(&request.content, &plan_tool_names) {
                let session_id = request.session_id.clone();
                let turn_id = request.target_task_id.clone().unwrap_or_default();
                let mode = policy
                    .granted_mode(&session_id)
                    .await
                    .map_err(|error| SessionError::new("invalid_input", error.to_string()))?;
                if mode == "plan" || mode.is_empty() {
                    // The escalation is OUT of plan mode, never a destination.
                    return Err(SessionError::invalid_input(
                        "a plan approval may not grant plan mode: it is the escalation out of it, not a destination.",
                    ));
                }
                match policy
                    .authorize(&PlanGrantIntent {
                        session_id: &session_id,
                        turn_id: &turn_id,
                        request_id: &request.input_id,
                        plan_digest: plan["planDigest"].as_str().unwrap_or_default(),
                        requested_mode: plan["requestedMode"].as_str(),
                    })
                    .await
                    .map_err(|error| SessionError::new("invalid_input", error.to_string()))?
                {
                    PlanAuthorization::Allowed => granted_mode = Some(mode),
                    PlanAuthorization::Denied(reason) => {
                        policy_denial = Some(reason.unwrap_or_else(|| "denied".to_owned()));
                    }
                }
            }
        }
        let outcome = self
            .store()
            .answer_human_request(AnswerHumanRequest {
                request_id: input.request_id.clone(),
                answer_id: input.answer_id.clone(),
                response: input.response.clone(),
                response_hash,
                now: self.now_ms(),
                message_id: self.mint("msg_"),
                plan_tool_names,
                policy_denial,
                granted_mode,
            })
            .await?;
        if outcome.enqueue.is_some() {
            self.wake();
        }
        Ok(outcome.receipt)
    }

    /// Approve exactly the call the request names (ADR 0019). `remember:
    /// true` also stores a standing approval for the same exact call, when
    /// the request offers one (`invalid_input` otherwise).
    pub async fn approve(
        &self,
        request_id: &str,
        answer_id: &str,
        note: Option<&str>,
        remember: Option<bool>,
    ) -> SessionResult<AnswerReceipt> {
        let mut response = Map::new();
        response.insert("decision".into(), "approve".into());
        if let Some(note) = note {
            response.insert("note".into(), note.into());
        }
        if let Some(remember) = remember {
            response.insert("remember".into(), remember.into());
        }
        self.answer(&AnswerInput {
            request_id: request_id.to_owned(),
            answer_id: answer_id.to_owned(),
            response: Value::Object(response),
        })
        .await
    }

    /// Answer the call with a denial result; it never runs.
    pub async fn deny(
        &self,
        request_id: &str,
        answer_id: &str,
        note: Option<&str>,
    ) -> SessionResult<AnswerReceipt> {
        let mut response = Map::new();
        response.insert("decision".into(), "deny".into());
        if let Some(note) = note {
            response.insert("note".into(), note.into());
        }
        self.answer(&AnswerInput {
            request_id: request_id.to_owned(),
            answer_id: answer_id.to_owned(),
            response: Value::Object(response),
        })
        .await
    }

    /// Every standing approval of the session, revoked ones included, newest
    /// first.
    pub async fn list_approval_grants(&self, session_id: &str) -> SessionResult<Vec<Value>> {
        let (chat, _) = self.require_session(session_id).await?;
        let mut grants = standing_grants(&chat.metadata);
        grants.reverse();
        Ok(grants)
    }

    /// Stop a standing approval from matching. Idempotent. Calls already
    /// running are not interrupted; the next matching call asks again.
    pub async fn revoke_approval_grant(
        &self,
        session_id: &str,
        grant_id: &str,
    ) -> SessionResult<Value> {
        self.require_session(session_id).await?;
        Ok(self
            .store()
            .revoke_standing_approval(session_id.to_owned(), grant_id.to_owned(), self.now_ms())
            .await?)
    }

    // -- deferred compaction, queue maintenance, configuration -------------------

    /// Ask for the session's context to be compacted (ADR 0017 part 2).
    /// DEFERRED: records `compactRequestedAt`; the session's NEXT turn
    /// performs it. Refused with `chat_busy` while a turn is live.
    pub async fn compact(&self, session_id: &str) -> SessionResult<()> {
        let (_, descriptor) = self.require_session(session_id).await?;
        self.store()
            .request_compact(
                session_id.to_owned(),
                session_turn_kind(&descriptor),
                self.now_ms(),
            )
            .await?;
        Ok(())
    }

    /// Pending inputs and the provider continuation are NOT copied.
    pub async fn fork(
        &self,
        session_id: &str,
        from_message_id: &str,
    ) -> SessionResult<AgentSessionState> {
        let (_, _) = self.require_session(session_id).await?;
        let forked_id = self.mint("chat_");
        self.store()
            .fork_chat(session_id, from_message_id, &forked_id, self.now_ms())
            .await?;
        let session_id = session_id.to_owned();
        let from = from_message_id.to_owned();
        let now = self.now_ms();
        self.store()
            .mutate_chat(&forked_id, now, move |chat| {
                let mut descriptor = read_session_descriptor(&chat.metadata)
                    .and_then(|descriptor| descriptor.as_object().cloned())
                    .ok_or_else(|| {
                        agentkit_store::StoreError::InvalidInput(
                            "forked descriptor unreadable".into(),
                        )
                    })?;
                descriptor.insert(
                    "createdAt".into(),
                    agentkit_store::ConversationTimestamp::from_epoch_millis(now)?
                        .wire_text()?
                        .into_owned()
                        .into(),
                );
                descriptor.insert(
                    "forkedFrom".into(),
                    json!({"sessionId": session_id, "messageId": from}),
                );
                let mut metadata = chat.metadata.as_object().cloned().unwrap_or_default();
                metadata.insert(SESSION_METADATA_KEY.into(), Value::Object(descriptor));
                let metadata = Value::Object(metadata);
                if read_session_descriptor(&metadata).is_none() {
                    return Err(agentkit_store::StoreError::InvalidInput(
                        "forked descriptor invalid".into(),
                    ));
                }
                chat.metadata = metadata;
                Ok(())
            })
            .await?;
        self.get_state(&forked_id).await
    }

    /// Future turns only; an active task keeps the model it was submitted with.
    pub async fn set_model(
        &self,
        session_id: &str,
        provider_id: Option<&str>,
        model: &str,
        reasoning_effort: Option<Option<&str>>,
    ) -> SessionResult<AgentSessionState> {
        if model.is_empty() {
            return Err(SessionError::invalid_input(
                "model must be a non-empty string.",
            ));
        }
        if let Some(Some(effort)) = &reasoning_effort
            && !REASONING_EFFORTS.contains(effort)
        {
            return Err(SessionError::invalid_input(format!(
                "reasoningEffort must be one of {}.",
                REASONING_EFFORTS.join(", ")
            )));
        }
        let model = model.to_owned();
        let provider_id = provider_id.map(str::to_owned);
        let effort = reasoning_effort.map(|value| value.map(str::to_owned));
        self.store()
            .mutate_chat(session_id, self.now_ms(), move |chat| {
                let descriptor = read_session_descriptor(&chat.metadata)
                    .and_then(|descriptor| descriptor.as_object().cloned())
                    .ok_or_else(|| {
                        agentkit_store::StoreError::NotFound(format!(
                            "Session {} is not an agent session.",
                            chat.id
                        ))
                    })?;
                let mut next: Map<String, Value> = descriptor
                    .into_iter()
                    .filter(|(key, _)| key != "reasoningEffort")
                    .collect();
                next.insert("model".into(), model.into());
                match &effort {
                    None => {}
                    Some(None) => {}
                    Some(Some(effort)) => {
                        next.insert("reasoningEffort".into(), effort.clone().into());
                    }
                }
                if let Some(provider_id) = &provider_id {
                    next.insert("providerId".into(), provider_id.clone().into());
                }
                let mut metadata = chat.metadata.as_object().cloned().unwrap_or_default();
                metadata.insert(SESSION_METADATA_KEY.into(), Value::Object(next));
                let metadata = Value::Object(metadata);
                if read_session_descriptor(&metadata).is_none() {
                    return Err(agentkit_store::StoreError::InvalidInput(
                        "descriptor invalid after model change".into(),
                    ));
                }
                chat.metadata = metadata;
                Ok(())
            })
            .await?;
        self.get_state(session_id).await
    }

    /// Cancel one pending queued follow-up or steer of the session before it
    /// is converted or placed. False when it is no longer pending; unknown or
    /// foreign ids are not found.
    pub async fn cancel_queued_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> SessionResult<bool> {
        self.require_session(session_id).await?;
        let record = self
            .store()
            .get_input(input_id.to_owned())
            .await?
            .filter(|record| {
                record.session_id == session_id
                    && matches!(
                        record.kind,
                        AgentInputKind::FollowUp | AgentInputKind::Steer
                    )
            })
            .ok_or_else(|| {
                SessionError::new(
                    "not_found",
                    format!("Queued input {input_id} not found in {session_id}."),
                )
            })?;
        if record.status != AgentInputStatus::Pending {
            return Ok(false);
        }
        Ok(self
            .store()
            .transition_input(
                input_id.to_owned(),
                vec![AgentInputStatus::Pending],
                AgentInputStatus::Cancelled,
                AgentInputPatch::default(),
                self.now_ms(),
            )
            .await
            .is_ok())
    }

    /// Cancel every pending queued follow-up and steer of the session. Human
    /// requests are untouched. Returns how many inputs were cancelled.
    pub async fn clear_queue(&self, session_id: &str) -> SessionResult<usize> {
        self.require_session(session_id).await?;
        let pending = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::FollowUp, AgentInputKind::Steer]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: None,
            })
            .await?;
        let mut cleared = 0;
        for record in pending {
            // A concurrent flush or cancel won this input; it is no longer queued.
            if self
                .store()
                .transition_input(
                    record.input_id,
                    vec![AgentInputStatus::Pending],
                    AgentInputStatus::Cancelled,
                    AgentInputPatch::default(),
                    self.now_ms(),
                )
                .await
                .is_ok()
            {
                cleared += 1;
            }
        }
        Ok(cleared)
    }

    /// Replace the session's Skills, context options and/or workspace trust
    /// flag (the root never changes). Future turns only; an active task keeps
    /// what it was submitted with.
    pub async fn configure(
        &self,
        session_id: &str,
        skills: Option<Vec<Value>>,
        context: Option<Value>,
        workspace_trusted: Option<bool>,
    ) -> SessionResult<AgentSessionState> {
        if workspace_trusted.is_some() {
            let (_, descriptor) = self.require_session(session_id).await?;
            if descriptor.get("workspace").is_none() {
                return Err(SessionError::invalid_input(
                    "workspaceTrusted needs a session with a workspace.",
                ));
            }
        }
        let skills = skills.map(Value::Array);
        self.store()
            .mutate_chat(session_id, self.now_ms(), move |chat| {
                let descriptor = read_session_descriptor(&chat.metadata)
                    .and_then(|descriptor| descriptor.as_object().cloned())
                    .ok_or_else(|| {
                        agentkit_store::StoreError::NotFound(format!(
                            "Session {} is not an agent session.",
                            chat.id
                        ))
                    })?;
                let mut next = descriptor;
                if let Some(trusted) = workspace_trusted
                    && let Some(workspace) =
                        next.get_mut("workspace").and_then(Value::as_object_mut)
                {
                    workspace.insert("trusted".into(), Value::Bool(trusted));
                }
                if let Some(skills) = &skills {
                    next.insert("skills".into(), skills.clone());
                }
                if let Some(context) = &context {
                    next.insert("context".into(), context.clone());
                }
                let mut metadata = chat.metadata.as_object().cloned().unwrap_or_default();
                metadata.insert(SESSION_METADATA_KEY.into(), Value::Object(next));
                let metadata = Value::Object(metadata);
                if read_session_descriptor(&metadata).is_none() {
                    return Err(agentkit_store::StoreError::InvalidInput(
                        "skills must be {id, revision} strings and context valid.".into(),
                    ));
                }
                chat.metadata = metadata;
                Ok(())
            })
            .await?;
        self.get_state(session_id).await
    }

    // -- internals -----------------------------------------------------------

    async fn require_session(&self, session_id: &str) -> SessionResult<(ChatRecord, Value)> {
        let chat = self.store().get_chat(session_id).await?;
        let descriptor = chat
            .as_ref()
            .and_then(|chat| read_session_descriptor(&chat.metadata));
        match (chat, descriptor) {
            (Some(chat), Some(descriptor)) => Ok((chat, descriptor)),
            _ => Err(SessionError::new(
                "session_not_found",
                format!("Session not found: {session_id}"),
            )),
        }
    }

    async fn require_turn(&self, session_id: &str, turn_id: &str) -> SessionResult<TaskRecord> {
        self.require_session(session_id).await?;
        self.store()
            .get_task(turn_id)
            .await?
            .filter(|task| task.scope_id == session_id)
            .ok_or_else(|| {
                SessionError::new(
                    "not_found",
                    format!("Turn {turn_id} not found in {session_id}."),
                )
            })
    }

    async fn active_turn(
        &self,
        session_id: &str,
        turn_kind: &str,
    ) -> SessionResult<Option<TaskRecord>> {
        Ok(self
            .store()
            .list_tasks(Some(session_id.to_owned()))
            .await?
            .into_iter()
            .find(|task| task.kind == turn_kind && !task.status.is_terminal()))
    }

    async fn cancel_requests(&self, session_id: &str, turn_id: &str) -> SessionResult<()> {
        let requests = self
            .store()
            .list_inputs(AgentInputQuery {
                session_id: session_id.to_owned(),
                kinds: Some(vec![AgentInputKind::HumanRequest]),
                statuses: Some(vec![AgentInputStatus::Pending]),
                target_task_id: Some(turn_id.to_owned()),
            })
            .await?;
        for request in requests {
            let _ = self
                .store()
                .transition_input(
                    request.input_id,
                    vec![AgentInputStatus::Pending],
                    AgentInputStatus::Cancelled,
                    AgentInputPatch::default(),
                    self.now_ms(),
                )
                .await;
        }
        Ok(())
    }

    fn plan_request_of(&self, record: &AgentInputRecord) -> Option<Value> {
        let names = self.deps.plan_approval.as_ref()?.tool_names();
        plan_request_of(&record.content, &names)
    }

    /// The queued input's turn task: derived from the input like a directly
    /// sent turn (`turn:<inputId>`), so a host can bind per-turn state to
    /// the input before it is converted. The caller mints the message ids so
    /// payload and records name the same pair.
    fn stage_task(
        &self,
        session_id: &str,
        descriptor: &Value,
        input_id: &str,
        user_message_id: &str,
        assistant_message_id: &str,
    ) -> SubmitTask {
        SubmitTask {
            task_id: turn_id_for(input_id),
            kind: session_turn_kind(descriptor),
            scope_id: session_id.to_owned(),
            payload: self.turn_payload(
                session_id,
                descriptor,
                user_message_id,
                assistant_message_id,
                None,
            ),
            priority: 0,
            now: self.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        }
    }

    fn turn_payload(
        &self,
        session_id: &str,
        descriptor: &Value,
        user_message_id: &str,
        assistant_message_id: &str,
        parent_message_id: Option<&str>,
    ) -> Value {
        let mut payload = Map::new();
        payload.insert("chatId".into(), session_id.into());
        if let Some(parent) = parent_message_id {
            payload.insert("parentMessageId".into(), parent.into());
        }
        if let Some(selection) = session_content_selection(descriptor) {
            payload.insert("contentSelection".into(), selection);
        }
        for key in ["model", "reasoningEffort", "providerId"] {
            if let Some(value) = descriptor.get(key) {
                payload.insert(key.into(), value.clone());
            }
        }
        payload.insert("assistantMessageId".into(), assistant_message_id.into());
        payload.insert("userMessageId".into(), user_message_id.into());
        Value::Object(payload)
    }

    fn stage_messages(
        &self,
        session_id: &str,
        task_id: &str,
        content: &Value,
        user_message_id: &str,
        assistant_message_id: &str,
        parent_message_id: Option<&str>,
    ) -> Vec<MessageInput> {
        vec![
            MessageInput {
                id: Some(user_message_id.to_owned()),
                chat_id: session_id.to_owned(),
                role: "user".into(),
                content: content.clone(),
                parent_message_id: parent_message_id.map(str::to_owned),
                ..MessageInput::default()
            },
            MessageInput {
                id: Some(assistant_message_id.to_owned()),
                chat_id: session_id.to_owned(),
                run_id: Some(task_id.to_owned()),
                role: "assistant".into(),
                content: Value::String(String::new()),
                parent_message_id: Some(user_message_id.to_owned()),
                metadata: json!({"placeholder": true}),
                ..MessageInput::default()
            },
        ]
    }

    /// Try to start a turn; if the session is busy, store the input instead.
    /// The attempt IS the idle check (the store refuses a concurrent turn),
    /// so there is no check-then-write window to lose a race in.
    async fn submit_or_queue(
        &self,
        session_id: &str,
        descriptor: &Value,
        kind: QueueKind,
        content: Value,
        intent: QueueIntent,
    ) -> SessionResult<AgentSendResult> {
        let turn_kind = session_turn_kind(descriptor);
        let mut busy: Option<SessionError> = None;
        for _attempt in 0..MAX_SUBMIT_ATTEMPTS {
            if let Some(existing) = &intent.input_id
                && let Some(prior) = self
                    .prior_receipt(session_id, kind, existing, &content)
                    .await?
            {
                return Ok(prior);
            }
            match self
                .submit_turn(session_id, descriptor, &content, intent.input_id.as_deref())
                .await
            {
                Ok(result) => return Ok(result),
                Err(error) if error.code == "chat_busy" => busy = Some(error),
                Err(error) => return Err(error),
            }
            let active = self.active_turn(session_id, &turn_kind).await?;
            // The turn settled between the refusal and now: start over as idle.
            let Some(active) = active else { continue };
            return self
                .queue_input(session_id, kind, content, intent, &active)
                .await;
        }
        Err(busy.unwrap_or_else(|| SessionError::chat_busy(session_id)))
    }

    async fn queue_input(
        &self,
        session_id: &str,
        kind: QueueKind,
        content: Value,
        intent: QueueIntent,
        active: &TaskRecord,
    ) -> SessionResult<AgentSendResult> {
        let QueueIntent {
            input_id,
            turn_id,
            on_settled,
        } = intent;
        let input_id =
            input_id.ok_or_else(|| SessionError::invalid_input("inputId is required to queue."))?;
        let target_task_id = if kind == QueueKind::Steer {
            let target = turn_id.clone().unwrap_or_else(|| active.task_id.clone());
            self.store()
                .get_task(&target)
                .await?
                .filter(|task| task.scope_id == session_id)
                .ok_or_else(|| {
                    SessionError::new(
                        "not_found",
                        format!("Turn {target} not found in {session_id}."),
                    )
                })?;
            Some(target)
        } else {
            None
        };
        let (record, _) = self
            .store()
            .create_input(
                CreateAgentInput {
                    input_id,
                    session_id: session_id.to_owned(),
                    kind: kind.input_kind(),
                    content: json!({"content": content.clone()}),
                    content_hash: self.hash_of(kind, &content),
                    target_task_id,
                    on_settled: on_settled
                        .map(AgentInputSettlePolicy::from)
                        .filter(|_| kind == QueueKind::Steer),
                    attempt_id: None,
                    tool_call_id: None,
                    expires_at: None,
                },
                self.now_ms(),
            )
            .await?;
        self.receipt_of(record).await
    }

    /// The earlier outcome of this `inputId`, or `None` when it is new.
    async fn prior_receipt(
        &self,
        session_id: &str,
        kind: QueueKind,
        input_id: &str,
        content: &Value,
    ) -> SessionResult<Option<AgentSendResult>> {
        if let Some(existing) = self.store().get_input(input_id.to_owned()).await? {
            // createInput is the idempotency check: same input replays, a
            // different one is `input_conflict`.
            let (record, _) = self
                .store()
                .create_input(
                    CreateAgentInput {
                        input_id: input_id.to_owned(),
                        session_id: session_id.to_owned(),
                        kind: kind.input_kind(),
                        content: json!({"content": content.clone()}),
                        content_hash: self.hash_of(kind, content),
                        target_task_id: existing.target_task_id.clone(),
                        on_settled: existing.on_settled,
                        attempt_id: None,
                        tool_call_id: None,
                        expires_at: None,
                    },
                    self.now_ms(),
                )
                .await?;
            return Ok(Some(self.receipt_of(record).await?));
        }
        let turn = self
            .store()
            .get_task(&turn_id_for(input_id))
            .await?
            .filter(|task| task.scope_id == session_id);
        Ok(turn.map(turn_result))
    }

    async fn receipt_of(&self, record: AgentInputRecord) -> SessionResult<AgentSendResult> {
        if record.status == AgentInputStatus::Converted
            && let Some(applied) = &record.applied_task_id
            && let Some(task) = self.store().get_task(applied).await?
        {
            return Ok(turn_result(task));
        }
        Ok(AgentSendResult::Queued {
            input: Box::new(record),
        })
    }

    fn hash_of(&self, kind: QueueKind, content: &Value) -> String {
        let envelope = json!({"kind": kind.as_str(), "content": content});
        sha256_hex(&serde_json::to_string(&envelope).unwrap_or_else(|_| "{}".into()))
    }

    async fn submit_turn(
        &self,
        session_id: &str,
        descriptor: &Value,
        content: &Value,
        input_id: Option<&str>,
    ) -> SessionResult<AgentSendResult> {
        let user_message_id = self.mint("msg_");
        let assistant_message_id = self.mint("msg_");
        let task_id = input_id
            .map(turn_id_for)
            .unwrap_or_else(|| self.mint("task_"));
        let payload = self.turn_payload(
            session_id,
            descriptor,
            &user_message_id,
            &assistant_message_id,
            None,
        );
        let task = SubmitTask {
            task_id: task_id.clone(),
            kind: session_turn_kind(descriptor),
            scope_id: session_id.to_owned(),
            payload,
            priority: 0,
            now: self.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        let messages = vec![
            MessageInput {
                id: Some(user_message_id.clone()),
                chat_id: session_id.to_owned(),
                role: "user".into(),
                content: content.clone(),
                ..MessageInput::default()
            },
            MessageInput {
                id: Some(assistant_message_id.clone()),
                chat_id: session_id.to_owned(),
                run_id: Some(task_id.clone()),
                role: "assistant".into(),
                content: Value::String(String::new()),
                parent_message_id: Some(user_message_id.clone()),
                metadata: json!({"placeholder": true}),
                ..MessageInput::default()
            },
        ];
        // The task goes in FIRST inside the store: it is the only write that
        // can reject, and a resubmitted key replays (identity-checked) rather
        // than answering the caller twice.
        let task = self.store().submit_turn(task, messages).await?;
        Ok(turn_result(task))
    }
}

fn turn_result(task: TaskRecord) -> AgentSendResult {
    AgentSendResult::Turn {
        turn_id: task.task_id,
        user_message_id: task.payload["userMessageId"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        assistant_message_id: task.payload["assistantMessageId"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    }
}

fn approval_of(record: &AgentInputRecord) -> Option<Value> {
    let content = record.content.as_object()?;
    if content.get("kind")?.as_str()? != "approval" {
        return None;
    }
    let tool_call_id = record.tool_call_id.as_ref()?;
    let args_sha256 = content.get("argsSha256")?.as_str()?;
    Some(json!({
        "requestId": record.input_id,
        "toolCallId": tool_call_id,
        "toolName": content.get("toolName").and_then(Value::as_str).unwrap_or(""),
        "argsSha256": args_sha256,
        "prompt": content.get("prompt").and_then(Value::as_str).unwrap_or(""),
    }))
}

/// The metadata key the next session turn compacts below the threshold and
/// clears (ADR 0017 part 2). Re-exported so host wiring can read it.
pub const COMPACT_REQUESTED_KEY: &str = COMPACT_REQUESTED_METADATA_KEY;

/// The approve marker `answer_human_request` leaves in `appliedMessageId`.
pub const APPROVAL_MARKER: &str = agentkit_store::APPROVE_MARKER;

#[async_trait]
impl crate::SessionFlushPlanner for AgentSessionService {
    async fn flush_candidates(
        &self,
        settled: &TaskRecord,
    ) -> crate::HostResult<Vec<StageQueuedTurn>> {
        // Only the session's own turn kind drives its queue; anything else
        // sharing the scope (or a non-session chat) is nobody's queue.
        let Ok((_, descriptor)) = self.require_session(&settled.scope_id).await else {
            return Ok(vec![]);
        };
        let turn_kind = session_turn_kind(&descriptor);
        if turn_kind != settled.kind {
            return Ok(vec![]);
        }
        Ok(self
            .queue_candidates(&settled.scope_id, &descriptor, &turn_kind)
            .await
            .unwrap_or_default())
    }
}
