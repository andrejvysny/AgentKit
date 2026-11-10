//! Durable native execution. Subscribers observe persisted events; dropping a
//! subscriber does not cancel the task that produced them.

mod budget;
mod chat;
mod continuation;
mod emulated_tool_call;
mod governance;
mod ports;
mod project_instructions;
mod proposals;
mod provider_registry;

pub use budget::{
    RoleTaskOverrides, parse_budget_limits, restore_execution_budget, task_overrides,
};
pub use chat::*;
pub use emulated_tool_call::{EMULATED_TOOL_CALL_MESSAGE, looks_like_emulated_tool_call};
pub use governance::*;
pub use ports::*;
pub use project_instructions::*;
pub use proposals::*;
pub use provider_registry::ProviderRegistry;

use agentkit_store::{SqliteStore, StoreError};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub use agentkit_engine::{
    AdmissionGate, ExecutionErrorClassification, ExecutionErrorKind, ExecutionFailure,
    ObserverError, RetryBackoff, RetrySafety, RunUpdate, Runtime, SessionFlushPlanner,
    SharedRuntime, SystemRuntime, TaskExecutor, TaskSettledObserver, VerificationHook,
    VerificationReport, WakeHandle, classify_execution_error,
};

pub use agentkit_sessions::{
    APPROVAL_MARKER, AgentHost, AgentLimits, AgentProfile, AgentResources, AgentSendResult,
    AgentSessionService, AgentSessionServiceDeps, AgentSessionState, AnswerInput, ApprovalGrantRef,
    CHAT_TURN_TASK_KIND, COMPACT_REQUESTED_KEY, CONTENT_UNSUPPORTED_MESSAGE, ChatProviderFactory,
    CodingPermissionAuthority, ContentAuthority, ContextUnit, ContextUnitKind, CreateSessionInput,
    ExecutionContext, FALLBACK_CONTEXT_WINDOW_TOKENS, FollowUpInput, FrozenRunContent, HostError,
    HostOptions, HostProviderPin, HostResult, OnSettled, PermissionContext, PlanApprovalPolicy,
    PlanAuthorization, PlanGrantIntent, ProfileRegistry, ProjectSelection, REASONING_EFFORTS,
    RECEIPT_RUN_ACTIVE, RunContentSelection, RunHandle, RunProfileDefaults, RunProfileService,
    SelectedSource, SelectedSourceKind, SendInput, SessionError, SessionExecutionAuthority,
    SessionResult, SkillSelection, SteerInput, TURN_CONFIG_KEYS, TaskProfileOverrides,
    TemplateSelection, TruncationPolicy, VerificationInput, WhenBusy, WorkerHandle,
    compiled_content_digest, freeze_run_content, is_valid_session_turn_kind, list_turn_tasks,
    read_session_descriptor, resolve_uncertain_execution, session_content_selection,
    session_turn_kind, session_usage, summarize_turn, turn_config, validate_frozen_run_content,
};
