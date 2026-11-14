//! The runtime's error surface. The binding maps it onto its stable public
//! codes; nothing here knows about any host language.

use agentkit_agents::AgentsError;
use agentkit_engine::EngineError;
use agentkit_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The caller's configuration or input was refused (`invalid_input`).
    #[error("{0}")]
    InvalidInput(String),
    /// A refusal that carries its own origin code (a coding-pack failure);
    /// the binding decides how the code crosses the boundary.
    #[error("{message}")]
    Coded { code: String, message: String },
    #[error("{0}")]
    Engine(#[from] EngineError<StoreError>),
    #[error("{0}")]
    Sessions(#[from] agentkit_sessions::SessionsError<StoreError>),
    #[error("{0}")]
    Chat(#[from] agentkit_chat::ChatError),
    /// Agents-context failures, including the unconfigured-lane sentinels
    /// the binding turns into stable public codes.
    #[error("{0}")]
    Agents(#[from] AgentsError),
}

impl RuntimeError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn typed_runtime_wrappers_expose_sources() {
        let error = RuntimeError::Sessions(agentkit_sessions::SessionsError::Store(
            agentkit_store::StoreError::LeaseLost("run".into()),
        ));
        let sessions = error
            .source()
            .unwrap()
            .downcast_ref::<agentkit_sessions::SessionsError<agentkit_store::StoreError>>()
            .unwrap();
        assert!(
            sessions
                .source()
                .unwrap()
                .is::<agentkit_store::StoreError>()
        );
        let error = RuntimeError::Agents(AgentsError::InvalidAgent("bad".into()));
        assert!(error.source().unwrap().is::<AgentsError>());
    }
}
