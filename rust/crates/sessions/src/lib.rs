//! Sessions context: the durable agent-session service, agent and run
//! profiles, and run-content freezing. Sits above the engine and below chat;
//! it holds no provider client and knows nothing about chat turns.

pub mod context_window;
mod ports;
mod profiles;
mod receipts;
mod run_content;
mod run_profiles;
mod session;
mod store_aliases;

pub use ports::ChatProviderFactory;
pub use profiles::{
    AgentLimits, AgentProfile, AgentResources, ProfileRegistry, SkillSelection, task_profile,
};
pub use receipts::resolve_uncertain_execution;
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
    CONTENT_UNSUPPORTED_MESSAGE, CodingPermissionAuthority, CreateSessionInput,
    FALLBACK_CONTEXT_WINDOW_TOKENS, FollowUpInput, OnSettled, PermissionContext,
    PlanApprovalPolicy, PlanAuthorization, PlanGrantIntent, REASONING_EFFORTS, RECEIPT_RUN_ACTIVE,
    SendInput, SessionError, SessionExecutionAuthority, SessionResult, SteerInput,
    TURN_CONFIG_KEYS, WhenBusy, is_valid_session_turn_kind, list_turn_tasks,
    read_session_descriptor, session_content_selection, session_turn_kind, session_usage,
    summarize_turn, turn_config,
};
pub use store_aliases::{
    AgentHost, ExecutionContext, HostError, HostOptions, HostResult, RunHandle, VerificationInput,
    WorkerHandle,
};
