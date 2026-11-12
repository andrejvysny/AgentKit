//! The agents context's error surface. Display texts are the contract: the
//! bindings forward them verbatim, so each variant renders exactly the
//! message the host raised for the same condition before the extraction.

use agentkit_core::CoreError;
use agentkit_engine::EngineError;
use agentkit_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum AgentsError {
    #[error("{0}")]
    Engine(#[from] EngineError<StoreError>),
    #[error("{0}")]
    Sessions(#[from] agentkit_sessions::SessionsError),
    #[error("{0}")]
    Chat(#[from] agentkit_chat::ChatError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{}: {}", .0.code, .0.message)]
    Core(#[from] CoreError),
    #[error("execution cancelled")]
    Cancelled,
    #[error("a parent may only await its own children")]
    ForeignChildAwait,
    #[error("advisor may not review an advisor run")]
    AdvisorRecursionRefused,
    #[error("advisor run failed (budget or provider)")]
    AdvisorRunFailed,
    #[error("advisor_disabled")]
    AdvisorDisabled,
    #[error("advisor_saturated")]
    AdvisorSaturated,
    #[error("chatId is required")]
    ChatIdRequired,
    #[error("invalid_reasoning_effort")]
    InvalidReasoningEffort,
    #[error("parent session carries no descriptor; only sessions delegate")]
    ParentSessionDescriptorMissing,
    #[error("parent session not found")]
    ParentSessionNotFound,
    #[error("parent turn already settled")]
    ParentTurnAlreadySettled,
    #[error("parent turn already settled; child cancelled")]
    ChildCancelledAfterParent,
    #[error("parent turn already settled; queued child cancelled")]
    QueuedChildCancelled,
    #[error("resume requires the caller's own live turn task")]
    ResumeParentTurnRequired,
    #[error("spawn requires the caller's own live turn task")]
    SpawnParentTurnRequired,
    #[error("subagent has no turn to resume")]
    SubagentHasNoTurnToResume,
    #[error("subagent prompt must not be empty")]
    SubagentPromptMustNotBeEmpty,
    #[error("subagent row vanished")]
    SubagentRowVanished,
    #[error("subagent_concurrency_limit: fan-out cap reached during registration")]
    RegistrationConcurrencyLimit,
    #[error("subagents_disabled")]
    SubagentsDisabled,
    #[error("agent_not_found: {0}")]
    AgentNotFound(String),
    #[error("subagent_not_found: {0}")]
    SubagentNotFound(String),
    #[error("{0}")]
    AdvisorModelRequired(String),
    #[error("{0}")]
    AdvisorProviderUnavailable(String),
    #[error("{0}")]
    AdvisorRateLimited(String),
    #[error("advisor_rate_limited: review budget unavailable: {0}")]
    AdvisorBudgetUnavailable(#[source] StoreError),
    #[error("child session id {0} is already in use by a different spawn")]
    ChildSessionConflict(String),
    #[error("{0}")]
    ModelNotAllowed(String),
    #[error("role {0} may not spawn children")]
    RoleCannotSpawn(String),
    #[error("spawn key {0} already exists with a different identity")]
    SpawnIdentityConflict(String),
    #[error("{0}")]
    SubagentBudgetExhausted(String),
    #[error("{0}")]
    SubagentConcurrencyLimitDetail(String),
    #[error("{0}")]
    SubagentDepthExceeded(String),
    #[error("{0}")]
    SubagentModelRequired(String),
    #[error("{0}")]
    SubagentParentLimit(String),
    #[error("{0}")]
    SubagentProviderUnavailable(String),
    #[error("subagent row vanished: {0}")]
    SubagentRowVanishedDetail(String),
    #[error("{0}")]
    SubagentTerminal(String),
    #[error("{0}")]
    SubagentTurnInvalid(String),
    #[error("{0}")]
    UnknownSubagentRole(String),
    #[error("{0}")]
    WorkspaceBusy(String),
    #[error("{0}")]
    WorkspaceRequired(String),
    #[error("{0}")]
    RoleDenied(#[source] RoleDenial),
    #[error("invalid_budget: {0}")]
    AdvisorInvalidBudget(#[source] agentkit_core::CoreError),
    #[error("advisor join: {0}")]
    AdvisorWorker(#[source] tokio::task::JoinError),
    #[error(
        "advisor_model_unconfigured: set advisor `model`/`watchdogModel` or review a session chat with a model"
    )]
    AdvisorModelUnconfigured,
    /// An agent definition failed semantic validation.
    #[error("invalid_agent: {0}")]
    InvalidAgent(String),
    /// A teammate admission request was malformed.
    #[error("invalid_admission: {0}")]
    InvalidAdmission(String),
    /// The subagent lane was never configured for this runtime.
    #[error("subagents not configured")]
    SubagentsNotConfigured,
    /// The advisor lane was never configured for this runtime.
    #[error("advisor not configured")]
    AdvisorNotConfigured,
}

pub type AgentsResult<T> = Result<T, AgentsError>;

impl From<AgentsError> for agentkit_governance::GovernanceError {
    fn from(error: AgentsError) -> Self {
        Self::Port(error.to_string())
    }
}

/// Why a child's authority resolves to deny-all: the spawn-time role
/// snapshot is unreadable, or its role is no longer configured. A refusal
/// reason, not a failure — the guard turns it into a tool refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoleDenial {
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error("subagent role {0} is no longer configured")]
    RoleGone(String),
}

/// A role snapshot present in a child's metadata but unusable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error("malformed role snapshot: {0}")]
    Malformed(String),
    #[error("unsupported role snapshot version {0}")]
    UnsupportedVersion(u32),
    #[error("role snapshot digest mismatch")]
    DigestMismatch,
}

/// An exclusive workspace lease is already held by another child.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("workspace lease held by {holder}")]
pub struct LeaseHeld {
    pub holder: String,
}

/// Task observer boundaries unwrap engine, store, and cancellation failures.
impl From<AgentsError> for EngineError<StoreError> {
    fn from(error: AgentsError) -> Self {
        use agentkit_engine::ContextClassification::{Rejected, Unclassified};
        match error {
            AgentsError::Engine(error) => error,
            AgentsError::Store(error) => Self::Store(error),
            AgentsError::Cancelled => Self::Cancelled,
            AgentsError::Chat(error) => error.into(),
            AgentsError::Sessions(error) => error.into(),
            error @ AgentsError::Core(_) => Self::External {
                failure_code: Some(error.failure_event_code()),
                source: Box::new(error),
            },
            error @ AgentsError::AdvisorWorker(_) => Self::External {
                failure_code: Some("advisor join".into()),
                source: Box::new(error),
            },
            error @ AgentsError::AdvisorBudgetUnavailable(_) => Self::External {
                failure_code: Some("advisor_rate_limited".into()),
                source: Box::new(error),
            },
            other => {
                let classification = match other {
                    AgentsError::InvalidAgent(_)
                    | AgentsError::InvalidAdmission(_)
                    | AgentsError::InvalidReasoningEffort
                    | AgentsError::AdvisorInvalidBudget(_) => Rejected,
                    _ => Unclassified,
                };
                Self::Context {
                    failure_code: Some(other.failure_event_code()),
                    source: Box::new(other),
                    classification,
                }
            }
        }
    }
}

impl AgentsError {
    fn failure_event_code(&self) -> String {
        match self {
            Self::Core(error) => error.code.clone(),
            Self::AgentNotFound(_) => "agent_not_found".into(),
            Self::SubagentNotFound(_) => "subagent_not_found".into(),
            Self::AdvisorModelRequired(_) => "advisor_model_required".into(),
            Self::AdvisorProviderUnavailable(_) => "advisor_provider_unavailable".into(),
            Self::AdvisorRateLimited(_) | Self::AdvisorBudgetUnavailable(_) => {
                "advisor_rate_limited".into()
            }
            Self::AdvisorInvalidBudget(_) => "invalid_budget".into(),
            Self::AdvisorWorker(_) => "advisor join".into(),
            Self::SubagentBudgetExhausted(_) => "subagent_budget_exhausted".into(),
            Self::SubagentConcurrencyLimitDetail(_) | Self::RegistrationConcurrencyLimit => {
                "subagent_concurrency_limit".into()
            }
            Self::SubagentDepthExceeded(_) => "subagent_depth_exceeded".into(),
            Self::SubagentModelRequired(_) => "subagent_model_required".into(),
            Self::SubagentParentLimit(_) => "subagent_parent_limit".into(),
            Self::SubagentProviderUnavailable(_) => "subagent_provider_unavailable".into(),
            Self::SubagentRowVanishedDetail(_) => "subagent row vanished".into(),
            Self::SubagentTerminal(_) => "subagent_terminal".into(),
            Self::UnknownSubagentRole(_) => "unknown_subagent_role".into(),
            Self::ModelNotAllowed(_) => "model_not_allowed".into(),
            Self::WorkspaceBusy(_) => "workspace_busy".into(),
            Self::WorkspaceRequired(_) => "workspace_required".into(),
            Self::InvalidAgent(_) => "invalid_agent".into(),
            Self::InvalidAdmission(_) => "invalid_admission".into(),
            Self::RoleCannotSpawn(id) => id_code("role ", id, " may not spawn children"),
            Self::SpawnIdentityConflict(id) => id_code(
                "spawn key ",
                id,
                " already exists with a different identity",
            ),
            Self::ChildSessionConflict(id) => id_code(
                "child session id ",
                id,
                " is already in use by a different spawn",
            ),
            Self::RoleDenied(RoleDenial::Snapshot(SnapshotError::Malformed(_))) => {
                "malformed role snapshot".into()
            }
            Self::RoleDenied(RoleDenial::Snapshot(SnapshotError::UnsupportedVersion(version))) => {
                format!("unsupported role snapshot version {version}")
            }
            Self::RoleDenied(RoleDenial::Snapshot(SnapshotError::DigestMismatch)) => {
                "role snapshot digest mismatch".into()
            }
            Self::RoleDenied(RoleDenial::RoleGone(id)) => {
                id_code("subagent role ", id, " is no longer configured")
            }
            other => other.to_string(),
        }
    }
}
fn id_code(prefix: &str, id: &str, suffix: &str) -> String {
    match id.split_once(':') {
        Some((head, _)) => format!("{prefix}{head}"),
        None => format!("{prefix}{id}{suffix}"),
    }
}

#[cfg(test)]
mod tests;
