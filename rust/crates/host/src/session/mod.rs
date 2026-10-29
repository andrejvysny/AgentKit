//! The durable agent-session service (ADR 0016-0019): the single owner of
//! session state, steering, follow-ups, human requests, plan approvals,
//! interruption and queueing. Port of `sessions/agent-session-service.ts`.
//!
//! The store is the only authority: every busy check and every queue
//! conversion is a single actor job, so there is no check-then-write window
//! for a concurrent submit to lose a race in. The service holds no provider
//! client. Descriptor settings the native dispatch path cannot honor —
//! selected Skills (content selection), provider reasoning effort, context
//! assembly overrides — are REFUSED before any turn effect with
//! `not_implemented` (never silently omitted), mirroring the HTTP boundary's
//! 501 for `contentSelection`; `compact` is likewise refused because no
//! native compaction runner consumes the request.

mod descriptor;
mod execution_authority;
mod report;

pub use descriptor::{
    CHAT_TURN_TASK_KIND, REASONING_EFFORTS, TURN_CONFIG_KEYS, is_valid_session_turn_kind,
    read_session_descriptor, session_content_selection, session_turn_kind, turn_config,
};
pub use execution_authority::{
    CodingPermissionAuthority, PermissionContext, SessionExecutionAuthority,
};
pub use report::{FALLBACK_CONTEXT_WINDOW_TOKENS, list_turn_tasks, session_usage, summarize_turn};

use crate::{HostResult, SharedRuntime};
use agentkit_store::{
    AgentInputKind, AgentInputPatch, AgentInputQuery, AgentInputRecord, AgentInputSettlePolicy,
    AgentInputStatus, AnswerHumanRequest, AnswerReceipt, COMPACT_REQUESTED_METADATA_KEY,
    ChatRecord, CreateAgentInput, MessageInput, QueueAdmission, QueueOrStage, SESSION_METADATA_KEY,
    STANDING_APPROVALS_METADATA_KEY, SqliteStore, StageQueuedTurn, StagedCandidate, SubmitTask,
    TaskRecord, TaskStatus, TurnStaging, execution_state_for_mode, plan_request_of,
    read_execution_state, standing_grants, with_execution,
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

/// The detail the HTTP boundary returns for a `contentSelection`
/// (`crates/host/src/http/turns.rs`: `501 not_implemented`), shared verbatim
/// with the session submit refusal so both boundaries carry one
/// code/message pair while native content resolution is unimplemented.
pub const CONTENT_UNSUPPORTED_MESSAGE: &str =
    "Native prompt templates and skills are not configured.";

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

/// Staging filter for queued inputs. Accepts exactly what the store's
/// converter can write (`convert_candidate`: a non-empty string or a
/// non-empty content-block array); anything else is skipped like malformed
/// content — a poison queued record must stay Pending and visible, never
/// abort a healthy turn's settle or a flush with an error.
fn read_input_content(content: &Value) -> Option<Value> {
    let value = content.get("content")?;
    let usable = match value {
        Value::String(text) => !text.is_empty(),
        Value::Array(blocks) => !blocks.is_empty(),
        _ => false,
    };
    usable.then(|| value.clone())
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
            // Malformed-present values (a scalar workspace, a string
            // windowTokens, a Skills entry without revision, an illegal
            // turnKind, …) invalidate the descriptor exactly like a bad
            // `profile.id` — they are never silently dropped.
            return Err(SessionError::invalid_input(
                "profile.id must be a non-empty string and workspace/context/skills/turnKind \
                 must match the session descriptor grammar.",
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
    /// Corrupt state is an ERROR, never absence: a caller falling back to
    /// its configured permissions would silently replace the human's
    /// recorded authority with whatever the process happens to allow.
    pub async fn get_execution(&self, session_id: &str) -> SessionResult<Option<Value>> {
        let (chat, _) = self.require_session(session_id).await?;
        match agentkit_store::inspect_execution_state(&chat.metadata) {
            agentkit_store::ExecutionStateRead::Absent => Ok(None),
            agentkit_store::ExecutionStateRead::Valid(state) => Ok(Some(state)),
            agentkit_store::ExecutionStateRead::Malformed => Err(SessionError::invalid_input(
                "The session's persisted execution state is malformed; refusing to fall back \
                 to configured permissions. Repair the record before granting access.",
            )),
        }
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
            .queue_candidates(session_id, &descriptor, &turn_kind, None)
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
    /// target has settled are cancelled on an explicit flush, never converted.
    /// Settlement plans leave that cancellation to their transaction. `settling`
    /// names a turn that is about to become terminal (the settle path):
    /// steers aimed at it are treated as target-terminal.
    async fn queue_candidates(
        &self,
        session_id: &str,
        descriptor: &Value,
        turn_kind: &str,
        settling: Option<&str>,
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
                if let Some(target) = &record.target_task_id
                    && Some(target.as_str()) != settling
                {
                    let target = self.store().get_task(target).await?;
                    if target
                        .as_ref()
                        .is_some_and(|task| !task.status.is_terminal())
                    {
                        continue; // aimed at a live turn: not this flush's candidate
                    }
                }
                if record.on_settled == Some(AgentInputSettlePolicy::Cancel) {
                    // Settlement owns this cancellation in its transaction.
                    // Planning may be retried or the settlement may fail, so
                    // taking its snapshot must have no durable side effects.
                    if settling.is_none() {
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
                    }
                    continue;
                }
            }
            // A steer converts under the SAME deterministic record id the
            // in-run projection uses, so every placement path for one input
            // is the same receipt — never two records for one steering.
            let Some(candidate) = self.stage_candidate(
                session_id,
                descriptor,
                &record.input_id,
                record.kind,
                &record.content,
            )?
            else {
                continue; // malformed content is skipped, like the TS logger path
            };
            out.push(StageQueuedTurn {
                session_id: session_id.to_owned(),
                task_kind: turn_kind.to_owned(),
                exclude_task_id: None,
                input_id: record.input_id.clone(),
                expected_kind: record.kind,
                task: candidate.task,
                messages: candidate.messages,
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
        // An already-answered request is an observation replay (or a
        // conflict the store refuses). It must not consult the plan-approval
        // policy: a one-shot decision would be spent twice, and a replay
        // must not depend on the grant provider still being reachable.
        if request.status == AgentInputStatus::Answered {
            let outcome = self
                .store()
                .answer_human_request(AnswerHumanRequest {
                    request_id: input.request_id.clone(),
                    answer_id: input.answer_id.clone(),
                    response: input.response.clone(),
                    response_hash,
                    now: self.now_ms(),
                    message_id: self.mint("msg_"),
                    plan_tool_names: vec![],
                    policy_denial: None,
                    granted_mode: None,
                })
                .await?;
            if outcome.enqueue.is_some() {
                self.wake();
            }
            return Ok(outcome.receipt);
        }
        let mut plan_tool_names = vec![];
        let mut policy_denial = None;
        let mut granted_mode = None;
        if let Some(policy) = &self.deps.plan_approval {
            plan_tool_names = policy.tool_names();
            // The human's decision is read BEFORE the grant policy: only an
            // approval escalates, so a denial never touches `granted_mode`
            // or `authorize`. A denial is the human's final word — the
            // request must persist (note included) even when the grant
            // service is unreachable, or the answer would fail, the request
            // stay pending and the parked turn wait forever. A response the
            // store will reject (no/other `decision`) also lands here: the
            // store validates the shape as its first write-free act.
            let approves =
                input.response.get("decision").and_then(Value::as_str) == Some("approve");
            if approves && let Some(plan) = plan_request_of(&request.content, &plan_tool_names) {
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
    /// REFUSED: nothing in the native executor consumes
    /// `compactRequestedAt` — the session's next turn performs no
    /// compaction and never clears the request — so recording it would be
    /// accept-but-ignore. Rejected with `not_implemented`, the same code the
    /// HTTP boundary uses for unconfigured optional services, until a
    /// compaction runner exists.
    pub async fn compact(&self, session_id: &str) -> SessionResult<()> {
        self.require_session(session_id).await?;
        Err(SessionError::new(
            "not_implemented",
            "Deferred session compaction is not configured on the native executor; the next turn performs no compaction.",
        ))
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

    /// Future turns only; an active task keeps the snapshot (model,
    /// provider) it was submitted with, and a patch never touches turns
    /// already queued.
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
                let mut next: Map<String, Value> = descriptor;
                next.insert("model".into(), model.into());
                // Three-state patch, matching TS setModel ("Omit to keep
                // the current effort; `null` falls back to the profile's."):
                // omitted preserves, explicit null clears, a value replaces.
                // `providerId` has no wire-level clear — TS replaces only —
                // so `Option<&str>` carries its full patch contract.
                match &effort {
                    None => {}
                    Some(None) => {
                        next.remove("reasoningEffort");
                    }
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

    /// THE staging contract for a queued input: the turn it becomes, built
    /// exactly as a submission made right now would build it — the CURRENT
    /// session descriptor's config (`turn_payload_base`), this session's turn
    /// kind, and ids derived from the input itself (`turn:<inputId>`,
    /// `steer:<inputId>` for a steer, `msg_q:`/`msg_a:` for a follow-up) so
    /// every path that promotes one input writes one identical receipt.
    ///
    /// Submission, an explicit queue flush, settlement promotion and the
    /// recovery of a half-admitted input all stage through here; the store's
    /// [`agentkit_store::TurnStaging`] carries the same config into the one
    /// promotion the host's snapshot cannot pre-stage. `None` is the staging
    /// filter: unusable content stays Pending and visible.
    fn stage_candidate(
        &self,
        session_id: &str,
        descriptor: &Value,
        input_id: &str,
        kind: AgentInputKind,
        content: &Value,
    ) -> SessionResult<Option<StagedCandidate>> {
        let Some(content) = read_input_content(content) else {
            return Ok(None);
        };
        let user_message_id = if kind == AgentInputKind::Steer {
            format!("steer:{input_id}")
        } else {
            format!("msg_q:{input_id}")
        };
        let assistant_message_id = format!("msg_a:{input_id}");
        let task_id = turn_id_for(input_id);
        let mut payload = self.turn_payload_base(session_id, descriptor)?;
        payload.insert(
            "assistantMessageId".into(),
            assistant_message_id.clone().into(),
        );
        payload.insert("userMessageId".into(), user_message_id.clone().into());
        let task = SubmitTask {
            task_id: task_id.clone(),
            kind: session_turn_kind(descriptor),
            scope_id: session_id.to_owned(),
            payload: Value::Object(payload),
            priority: 0,
            now: self.now_ms(),
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: true,
        };
        Ok(Some(StagedCandidate {
            input_id: input_id.to_owned(),
            task,
            messages: self.stage_messages(
                session_id,
                &task_id,
                &content,
                &user_message_id,
                &assistant_message_id,
                None,
            ),
        }))
    }

    /// The authoritative config the store needs to stage what a settlement's
    /// snapshot could not name: the same contract [`Self::stage_candidate`]
    /// applies. The transaction checks the persisted descriptor before
    /// stamping it, so a concurrent patch forces the host to re-plan.
    fn turn_staging(&self, session_id: &str, descriptor: &Value) -> SessionResult<TurnStaging> {
        Ok(TurnStaging {
            kind: session_turn_kind(descriptor),
            config: Value::Object(self.turn_payload_base(session_id, descriptor)?),
        })
    }

    /// The task kind this session's turns run under, plus the immutable
    /// snapshot every turn carries.
    ///
    /// REFUSES, before any turn effect, descriptor settings the native
    /// dispatch path cannot honor: selected Skills (content selection — the
    /// executor never resolves it, so dispatching would silently omit the
    /// selected instructions), provider reasoning effort (the provider
    /// request carries no effort field; it would vanish), and context
    /// assembly overrides (policy, token budgets, tool-result elision — the
    /// executor assembles history from its own options). This mirrors the
    /// HTTP boundary's refusal of `contentSelection` (`http/turns.rs`:
    /// `501 not_implemented`, "Native prompt templates and skills are not
    /// configured.") in code and message shape.
    fn turn_payload(
        &self,
        session_id: &str,
        descriptor: &Value,
        user_message_id: &str,
        assistant_message_id: &str,
        parent_message_id: Option<&str>,
    ) -> SessionResult<Value> {
        let mut payload = self.turn_payload_base(session_id, descriptor)?;
        if let Some(parent) = parent_message_id {
            payload.insert("parentMessageId".into(), parent.into());
        }
        payload.insert("assistantMessageId".into(), assistant_message_id.into());
        payload.insert("userMessageId".into(), user_message_id.into());
        Ok(Value::Object(payload))
    }

    /// The config half of the staging contract: every turn of this session
    /// carries these keys, captured from the descriptor AT TASK CREATION,
    /// whoever stages it — submission, an explicit flush, a settlement's
    /// promotion, or the recovery of a half-admitted input. The refusal
    /// checks live here so no path can stage a turn the native dispatch path
    /// cannot honor.
    fn turn_payload_base(
        &self,
        session_id: &str,
        descriptor: &Value,
    ) -> SessionResult<Map<String, Value>> {
        if session_content_selection(descriptor).is_some() {
            return Err(SessionError::new(
                "not_implemented",
                CONTENT_UNSUPPORTED_MESSAGE,
            ));
        }
        if descriptor.get("reasoningEffort").is_some() {
            return Err(SessionError::new(
                "not_implemented",
                "Provider reasoning effort is not configured on the native executor.",
            ));
        }
        if descriptor.get("context").is_some() {
            return Err(SessionError::new(
                "not_implemented",
                "Session context assembly overrides are not configured on the native executor.",
            ));
        }
        let mut payload = Map::new();
        payload.insert("chatId".into(), session_id.into());
        for (key, value) in turn_config(descriptor) {
            payload.insert(key, value);
        }
        Ok(payload)
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

    /// Try to start a turn; if the session is busy, admit the input instead.
    /// The submit attempt IS the idle check (the store refuses a concurrent
    /// turn), and the queueing half is ONE store write that re-checks the
    /// active turn as it inserts — so there is no check-then-write window
    /// anywhere for a settlement to slip between. An input can never be left
    /// Pending behind a turn that settled after a read and before an insert.
    async fn submit_or_queue(
        &self,
        session_id: &str,
        descriptor: &Value,
        kind: QueueKind,
        content: Value,
        intent: QueueIntent,
    ) -> SessionResult<AgentSendResult> {
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
            Ok(result) => {
                self.wake();
                Ok(result)
            }
            Err(error) if error.code == "chat_busy" => {
                self.enqueue_input(session_id, descriptor, kind, content, intent)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    /// Admit a queued input. `submit_turn`'s `chat_busy` is what routes here,
    /// so the session WAS busy moments ago; the store's own check inside the
    /// admission transaction — not a read this call cannot keep true —
    /// decides whether the input waits behind the live turn or becomes the
    /// next turn itself, and the answer commits with the insertion. The
    /// reread-then-insert pair this replaces is exactly how a follow-up that
    /// landed after a settlement's commit stayed Pending forever with no
    /// active turn and an idle worker.
    async fn enqueue_input(
        &self,
        session_id: &str,
        descriptor: &Value,
        kind: QueueKind,
        content: Value,
        intent: QueueIntent,
    ) -> SessionResult<AgentSendResult> {
        let QueueIntent {
            input_id,
            turn_id,
            on_settled,
        } = intent;
        let input_id =
            input_id.ok_or_else(|| SessionError::invalid_input("inputId is required to queue."))?;
        // An explicit target is validated up front (a steer aimed at a turn
        // that never existed is `not_found`); with none, the store names the
        // live turn it actually finds, so the running turn sees its steer at
        // the next boundary instead of promoting it blind.
        let target_task_id = match (&turn_id, kind) {
            (Some(target), QueueKind::Steer) => {
                self.store()
                    .get_task(target)
                    .await?
                    .filter(|task| task.scope_id == session_id)
                    .ok_or_else(|| {
                        SessionError::new(
                            "not_found",
                            format!("Turn {target} not found in {session_id}."),
                        )
                    })?;
                Some(target.clone())
            }
            _ => None,
        };
        // The durable envelope is what the staging filter reads too, so the
        // turn an admission can start is staged from exactly the bytes the
        // input stores (`queue_content` / `read_input_content` shape).
        let envelope = json!({"content": content});
        let candidate = self.stage_candidate(
            session_id,
            descriptor,
            &input_id,
            kind.input_kind(),
            &envelope,
        )?;
        let content_hash = self.hash_of(kind, &envelope["content"]);
        let admission = self
            .store()
            .queue_or_stage_turn(QueueOrStage {
                session_id: session_id.to_owned(),
                task_kind: session_turn_kind(descriptor),
                input: CreateAgentInput {
                    input_id,
                    session_id: session_id.to_owned(),
                    kind: kind.input_kind(),
                    content: envelope,
                    content_hash,
                    target_task_id,
                    on_settled: on_settled
                        .map(AgentInputSettlePolicy::from)
                        .filter(|_| kind == QueueKind::Steer),
                    attempt_id: None,
                    tool_call_id: None,
                    expires_at: None,
                },
                target_the_live_turn: kind == QueueKind::Steer && turn_id.is_none(),
                candidate,
                now: self.now_ms(),
            })
            .await?;
        match admission {
            QueueAdmission::Staged(task) => {
                // This write created the next turn: rouse a worker for it.
                self.wake();
                Ok(turn_result(task))
            }
            QueueAdmission::Queued(record) => {
                // Durable behind a live turn whose settlement transaction
                // re-reads the queue and owns the promotion.
                self.receipt_of(record).await
            }
        }
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
        )?;
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

/// The metadata key a compaction request would record (ADR 0017 part 2).
/// Re-exported so host wiring can read it; the native service currently
/// REFUSES `compact` because no native turn consumes or clears this marker.
pub const COMPACT_REQUESTED_KEY: &str = COMPACT_REQUESTED_METADATA_KEY;

/// The approve marker `answer_human_request` leaves in `appliedMessageId`.
pub const APPROVAL_MARKER: &str = agentkit_store::APPROVE_MARKER;

#[async_trait]
impl crate::SessionFlushPlanner for AgentSessionService {
    async fn settle_plan(
        &self,
        settling: &TaskRecord,
        completed: bool,
    ) -> crate::HostResult<crate::SettlePlan> {
        let none = || crate::SettlePlan {
            expected_descriptor: None,
            staging: None,
            convert: vec![],
        };
        // A failed or cancelled turn promotes nothing: its queued inputs
        // stay pending and visible until an explicit flushQueue
        // (ADR 0016 §4).
        if !completed {
            return Ok(none());
        }
        // Only the session's own turn kind drives its queue; anything else
        // sharing the scope (or a non-session chat) is nobody's queue.
        let Ok((chat, descriptor)) = self.require_session(&settling.scope_id).await else {
            return Ok(none());
        };
        let turn_kind = session_turn_kind(&descriptor);
        if turn_kind != settling.kind {
            return Ok(none());
        }
        // THE authoritative config for anything this snapshot cannot name,
        // read from the descriptor NOW. A `setModel` that landed while this
        // turn ran must reach the turn THIS settle promotes; templating the
        // missed staging off the settling task's payload handed it the model
        // an earlier submission was made with. A descriptor the native
        // dispatch path cannot honor stages nothing (fail-closed, exactly
        // like the candidate filter below) rather than half-stamping a turn.
        let staging = self.turn_staging(&settling.scope_id, &descriptor).ok();
        // At settling time the settling turn itself counts as terminal: a
        // steer aimed at it that was never placed is a promotion candidate
        // (after the follow-ups). The whole eligible queue goes to the store
        // in arrival order; the settlement transaction CAS-validates each
        // against the state it commits and converts the first winner, so a
        // candidate cancelled after this snapshot cannot strand the ones
        // behind it.
        let candidates = self
            .queue_candidates(
                &settling.scope_id,
                &descriptor,
                &turn_kind,
                Some(&settling.task_id),
            )
            .await
            .unwrap_or_default();
        Ok(crate::SettlePlan {
            expected_descriptor: Some(chat.metadata[SESSION_METADATA_KEY].clone()),
            staging,
            convert: candidates
                .into_iter()
                .map(|candidate| agentkit_store::StagedCandidate {
                    input_id: candidate.input_id,
                    task: candidate.task,
                    messages: candidate.messages,
                })
                .collect(),
        })
    }
}
