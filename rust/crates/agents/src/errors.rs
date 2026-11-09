//! The agents context's error surface. Display texts are the contract: the
//! bindings forward them verbatim, so each variant renders exactly the
//! message the host raised for the same condition before the extraction.

use agentkit_core::CoreError;
use agentkit_host::HostError;
use agentkit_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum AgentsError {
    #[error(transparent)]
    Host(#[from] HostError),
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

impl AgentsError {
    pub(crate) fn execution(message: String) -> Self {
        Self::Host(HostError::Execution(message))
    }

    pub(crate) fn not_found(message: String) -> Self {
        Self::Host(HostError::NotFound(message))
    }

    pub(crate) fn cancelled() -> Self {
        Self::Host(HostError::Cancelled)
    }
}

impl From<StoreError> for AgentsError {
    fn from(error: StoreError) -> Self {
        Self::Host(error.into())
    }
}

impl From<CoreError> for AgentsError {
    fn from(error: CoreError) -> Self {
        Self::execution(format!("{}: {}", error.code, error.message))
    }
}

impl From<AgentsError> for agentkit_governance::GovernanceError {
    fn from(error: AgentsError) -> Self {
        Self::Execution(error.to_string())
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

/// Trait boundaries the host owns (`TaskSettledObserver`) speak `HostError`.
impl From<AgentsError> for HostError {
    fn from(error: AgentsError) -> Self {
        match error {
            AgentsError::Host(inner) => inner,
            other => Self::Execution(other.to_string()),
        }
    }
}
