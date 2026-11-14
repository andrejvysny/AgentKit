//! Sessions context: the durable agent-session service, agent and run
//! profiles, and run-content freezing. Sits above the engine and below chat;
//! it holds no provider client and knows nothing about chat turns.
//!
//! The context owns the port it drives ([`SessionRepository`]) and the
//! records it speaks in ([`model`], [`metadata`]) per ADR 0024; it names no
//! concrete store.

pub mod context_window;
mod error;
mod failure_code;
mod metadata;
mod model;
mod ports;
mod profiles;
mod receipts;
mod repository;
mod run_content;
mod run_profiles;
mod session;

pub use agentkit_engine::MessageInput;
pub use error::{
    AnswerStoreError, LegacySessionDebug, ReceiptStoreError, SessionStoreError, SessionsError,
    SessionsResult,
};
pub use metadata::{
    APPROVE_MARKER, COMPACT_REQUESTED_METADATA_KEY, Decision, EXECUTION_METADATA_KEY,
    ExecutionStateRead, MAX_STANDING_APPROVALS, SESSION_METADATA_KEY,
    STANDING_APPROVALS_METADATA_KEY, SessionMutation, SessionRefusal, apply_session_mutation,
    approval_answer_refusal, execution_state_for_grant, execution_state_for_mode,
    find_active_standing_grant, inspect_execution_state, is_approval_request, is_hex64, make_room,
    parse_answer_expiry, plan_request_of, read_decision, read_execution_state, read_standing_key,
    standing_grants, with_execution, with_standing, writable_standing_grants,
};
pub use model::{
    AgentBudgets, AgentDefinition, AgentInputKind, AgentInputPatch, AgentInputQuery,
    AgentInputRecord, AgentInputSettlePolicy, AgentInputStatus, AgentLifecycle, AgentRecord,
    AgentRevisionSource, AgentSkillSelection, AnswerHumanRequest, AnswerOutcome, AnswerReceipt,
    ArgsHashProvenance, ChatRecord, CreateAgentInput, ExecutionReceiptRecord,
    ExecutionReceiptStatus, ExecutionResolution, MessageRecord, PROVIDER_MODELS_NAMESPACE,
    QueueAdmission, QueueOrStage, RECEIPT_NOT_UNCERTAIN, RECEIPT_RUN_ACTIVE, ResolutionOutcome,
    ResolveExecution, SessionSettle, StageQueuedTurn, StagedCandidate, TurnStaging,
    parse_iso_millis, run_profile_digest,
};
pub use ports::ChatProviderFactory;
pub use profiles::{
    AgentLimits, AgentProfile, AgentResources, ProfileRegistry, SkillSelection, task_profile,
};
pub use receipts::resolve_uncertain_execution;
pub use repository::{RepositoryFault, SessionBackend, SessionFault, SessionRepository};
pub use run_content::{
    ContentAuthority, FrozenRunContent, RunContentSelection, TemplateSelection, freeze_run_content,
    selected_messages, validate_frozen_run_content,
};
pub use run_profiles::{
    ApprovalGrantRef, ContextUnit, ContextUnitKind, HostProviderPin, ProjectSelection,
    RunProfileDefaults, RunProfileService, SelectedSource, SelectedSourceKind,
    TaskProfileOverrides, TruncationPolicy, compiled_content_digest,
};
pub use session::{
    APPROVAL_MARKER, AgentSendResult, AgentSessionService, AgentSessionServiceDeps,
    AgentSessionState, AnswerInput, CHAT_TURN_TASK_KIND, COMPACT_REQUESTED_KEY,
    CONTENT_UNSUPPORTED_MESSAGE, CreateSessionInput, FALLBACK_CONTEXT_WINDOW_TOKENS, FollowUpInput,
    OnSettled, PlanApprovalPolicy, PlanAuthorization, PlanGrantIntent, REASONING_EFFORTS,
    SendInput, SteerInput, TURN_CONFIG_KEYS, WhenBusy, is_valid_session_turn_kind, list_turn_tasks,
    read_session_descriptor, session_content_selection, session_turn_kind, session_usage,
    summarize_turn, turn_config,
};
