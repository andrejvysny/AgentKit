//! The durable agent-session service (ADR 0016-0019): the single owner of
//! session state, steering, follow-ups, human requests, plan approvals,
//! interruption and queueing. Port of `sessions/agent-session-service.ts`.
//!
//! The store is the only authority: every busy check and every queue
//! conversion is a single actor job, so there is no check-then-write window
//! for a concurrent submit to lose a race in. The service holds no provider
//! client. Profiles and selected managed content are resolved before admission,
//! then carried as immutable snapshots. Reasoning and context settings travel
//! through the native executor to provider dispatch.

mod admission;
mod answer;
mod control;
mod descriptor;
mod input;
mod lifecycle;
mod receipts;
mod report;
mod staging;

pub use descriptor::{
    CHAT_TURN_TASK_KIND, REASONING_EFFORTS, TURN_CONFIG_KEYS, is_valid_session_turn_kind,
    read_session_descriptor, session_content_selection, session_turn_kind, turn_config,
};
pub use report::{FALLBACK_CONTEXT_WINDOW_TOKENS, list_turn_tasks, session_usage, summarize_turn};

use crate::{
    AgentInputKind, AgentInputRecord, AgentInputSettlePolicy, COMPACT_REQUESTED_METADATA_KEY,
    SessionBackend, SessionFault, SessionsError, SessionsResult,
};
use agentkit_engine::{SharedRuntime, TaskRecord};
use agentkit_kernel::ConversationTimestamp;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// Legacy message retained for source compatibility; native content admission
/// now resolves immutable snapshots and no longer emits this refusal.
pub const CONTENT_UNSUPPORTED_MESSAGE: &str =
    "Native prompt templates and skills are not configured.";

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
    /// `plan` or the empty string denies. A policy is host logic and drives no
    /// repository, so it reports a faultless refusal.
    async fn granted_mode(&self, session_id: &str) -> SessionsResult<String>;
    /// Extra veto (role ceiling, untrusted workspace).
    async fn authorize(&self, _intent: &PlanGrantIntent<'_>) -> SessionsResult<PlanAuthorization> {
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
    pub reasoning_effort: Option<agentkit_core::ReasoningEffort>,
    pub content_selection: Option<crate::RunContentSelection>,
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
    /// Per-turn frozen native admission stanza (`executionTarget`,
    /// `turnInput`, `broker`, `continuationScope`). Validated BEFORE any row
    /// lands — a malformed stanza is refused at SEND, never discovered at
    /// claim (AGENTKIT-112: no native validation after queueing). Queued
    /// with the input, so flush/settlement promotion carry the identical
    /// bytes.
    pub frozen_payload: Option<Map<String, Value>>,
}

#[derive(Clone, Debug)]
pub struct FollowUpInput {
    pub content: Value,
    pub input_id: Option<String>,
    pub frozen_payload: Option<Map<String, Value>>,
}

#[derive(Clone, Debug)]
pub struct SteerInput {
    pub content: Value,
    pub input_id: Option<String>,
    pub turn_id: Option<String>,
    pub on_settled: Option<OnSettled>,
    pub frozen_payload: Option<Map<String, Value>>,
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

pub struct AgentSessionServiceDeps<R: SessionBackend> {
    pub store: R,
    pub runtime: SharedRuntime,
    /// Plan-mode escalation policy. Absent, plan approvals are never honored
    /// and a session in plan mode stays read-only.
    pub plan_approval: Option<Arc<dyn PlanApprovalPolicy>>,
    /// Best-effort queue poke (the TS `taskRunner.enqueue`); polling still
    /// covers every miss, so `None` only costs latency.
    pub wake: Option<agentkit_engine::WakeHandle>,
}

#[derive(Clone)]
struct Deps<R: SessionBackend> {
    store: R,
    runtime: SharedRuntime,
    plan_approval: Option<Arc<dyn PlanApprovalPolicy>>,
    wake: Option<agentkit_engine::WakeHandle>,
    /// The host's executor registry, when this service fronts a host whose
    /// worker will run its turns. `None` (the constructor's default) leaves
    /// admission unfiltered — for wiring where no native worker consumes the
    /// queue at all. When set, every admission that would create a task
    /// refuses an unregistered kind BEFORE any row lands.
    gate: Option<agentkit_engine::AdmissionGate>,
    profiles: Arc<crate::ProfileRegistry>,
    provider_factory: Option<Arc<dyn crate::ChatProviderFactory>>,
    default_provider_id: Option<String>,
    default_model: Option<String>,
    content_authority: Option<Arc<dyn crate::ContentAuthority<SessionFault<R>>>>,
}

/// The session service. Cheap to clone; all state lives in the repository.
#[derive(Clone)]
pub struct AgentSessionService<R: SessionBackend> {
    deps: Arc<Deps<R>>,
}

/// A stale-answer refusal carrying the TypeScript `AnswerStaleError` text
/// verbatim: `Request <id> can no longer be answered: <reason>.`
fn answer_stale<E: crate::RepositoryFault>(request_id: &str, reason: &str) -> SessionsError<E> {
    SessionsError::answer_stale(format!(
        "Request {request_id} can no longer be answered: {reason}."
    ))
}

/// Preserve the adapter's typed stale-answer refusal and source.
fn answer_store_error<E: crate::RepositoryFault>(error: E) -> SessionsError<E> {
    SessionsError::AnswerStore(crate::AnswerStoreError(error))
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

impl<R: SessionBackend> AgentSessionService<R> {
    pub fn new(deps: AgentSessionServiceDeps<R>) -> Self {
        Self {
            deps: Arc::new(Deps {
                store: deps.store,
                runtime: deps.runtime,
                plan_approval: deps.plan_approval,
                wake: deps.wake,
                gate: None,
                profiles: Arc::new(crate::ProfileRegistry::default()),
                provider_factory: None,
                default_provider_id: None,
                default_model: None,
                content_authority: None,
            }),
        }
    }

    /// Bind the host's admission registry: every admission that creates a
    /// task (turn submit, queue-or-stage, flush promotion) then refuses a
    /// kind with no registered `TaskExecutor` with the stable
    /// `no_executor_for_kind` code, BEFORE the store transaction begins —
    /// so a refused admission leaves zero rows: no task, no messages, no
    /// converted/queued input. Whole admissions are rejected, never
    /// admitted-except-the-missing-one (the store admits an input's task
    /// atomically with its messages and receipt; there is no partial batch
    /// left to keep or drop). Late wiring is deliberate: the binding code
    /// builds the host and the service from the same store and links them
    /// here, exactly as it hands the host the service's flush planner.
    pub fn with_gate(mut self, gate: agentkit_engine::AdmissionGate) -> Self {
        let mut deps = self.deps.as_ref().clone();
        deps.gate = Some(gate);
        self.deps = Arc::new(deps);
        self
    }

    pub fn with_profiles(mut self, profiles: Arc<crate::ProfileRegistry>) -> Self {
        let mut deps = self.deps.as_ref().clone();
        deps.profiles = profiles;
        self.deps = Arc::new(deps);
        self
    }

    /// The registry admission resolves session profiles against. The HTTP
    /// boundary serves chats as sessions too, so it resolves them against
    /// THIS registry: a consumer that declared profiles for its service
    /// declares them once, and a profile it owns is never a 400 at the
    /// boundary that admits it through `create_session_chat`.
    pub fn profiles(&self) -> Arc<crate::ProfileRegistry> {
        Arc::clone(&self.deps.profiles)
    }

    pub fn with_defaults(mut self, provider_id: Option<String>, model: Option<String>) -> Self {
        let mut deps = self.deps.as_ref().clone();
        deps.default_provider_id = provider_id;
        deps.default_model = model;
        self.deps = Arc::new(deps);
        self
    }

    pub fn with_provider_factory(mut self, factory: Arc<dyn crate::ChatProviderFactory>) -> Self {
        let mut deps = self.deps.as_ref().clone();
        deps.provider_factory = Some(factory);
        self.deps = Arc::new(deps);
        self
    }

    pub fn with_content_authority(
        mut self,
        authority: Arc<dyn crate::ContentAuthority<SessionFault<R>>>,
    ) -> Self {
        let mut deps = self.deps.as_ref().clone();
        deps.content_authority = Some(authority);
        self.deps = Arc::new(deps);
        self
    }

    /// The registry check every task-creating admission runs before its
    /// first write. Ungated wirings (no native worker bound) admit freely.
    fn require_executable(&self, kind: &str) -> SessionsResult<(), SessionFault<R>> {
        if self
            .deps
            .gate
            .as_ref()
            .is_some_and(|gate| !gate.supports(kind))
        {
            return Err(SessionsError::no_executor_for_kind(format!(
                "No executor registered for task kind \"{kind}\"."
            )));
        }
        Ok(())
    }

    fn store(&self) -> &R {
        &self.deps.store
    }
    fn now_ms(&self) -> i64 {
        self.deps.runtime.now_ms()
    }
    fn mint(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.deps.runtime.id())
    }
    fn now_iso(&self) -> String {
        ConversationTimestamp::from_epoch_millis(self.now_ms())
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
pub const APPROVAL_MARKER: &str = crate::APPROVE_MARKER;
