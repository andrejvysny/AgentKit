//! The runtime's error surface. The binding maps it onto its stable public
//! codes; nothing here knows about any host language.

use agentkit_agents::AgentsError;
use agentkit_sessions::HostError;

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
    /// Agents-context failures, including the unconfigured-lane sentinels
    /// the binding turns into stable public codes.
    #[error(transparent)]
    Agents(#[from] AgentsError),
}

impl RuntimeError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
}
