//! The runtime's error surface. The binding maps it onto its stable public
//! codes; nothing here knows about any host language.

use agentkit_host::HostError;

/// Sentinel texts the late-bound lanes raise through `HostError::Execution`
/// when a subsystem was never configured; the binding turns them into stable
/// public codes instead of an opaque `internal_error`.
pub const SUBAGENTS_NOT_CONFIGURED: &str = "subagents not configured";
pub const ADVISOR_NOT_CONFIGURED: &str = "advisor not configured";

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The caller's configuration or input was refused (`invalid_input`).
    #[error("{0}")]
    InvalidInput(String),
    /// A refusal that carries its own origin code (a coding-pack failure);
    /// the binding decides how the code crosses the boundary.
    #[error("{message}")]
    Coded { code: String, message: String },
    #[error(transparent)]
    Host(#[from] HostError),
}

impl RuntimeError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
}
