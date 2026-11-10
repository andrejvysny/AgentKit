use crate::ExecutionFailure;

#[derive(Debug, thiserror::Error)]
pub enum EngineError<E: std::error::Error> {
    #[error(transparent)]
    Failure(#[from] ExecutionFailure),
    #[error(transparent)]
    Store(E),
    #[error("task executor not registered: {0}")]
    UnknownKind(String),
    /// A task admission named a kind this host registered no executor for.
    /// Admission is refused with this stable code BEFORE any row lands, so
    /// the queue never holds work this host has promised to run and never
    /// can; the store-level APIs stay unfiltered for deliberate cross-pool
    /// seeding, and the runner's claim filter keeps such rows invisible to
    /// this host either way.
    #[error("no executor registered for task kind: {0}")]
    NoExecutorForKind(String),
    #[error("execution cancelled")]
    Cancelled,
    #[error("{0}")]
    Execution(String),
    /// A named durable record does not exist; the message carries the
    /// stable code (e.g. `subagent_not_found: <id>`).
    #[error("{0}")]
    NotFound(String),
}

impl<E: std::error::Error> From<agentkit_governance::GovernanceError> for EngineError<E> {
    fn from(error: agentkit_governance::GovernanceError) -> Self {
        match error {
            agentkit_governance::GovernanceError::Execution(message) => Self::Execution(message),
        }
    }
}

impl<E: std::error::Error> From<EngineError<E>> for agentkit_governance::GovernanceError {
    fn from(error: EngineError<E>) -> Self {
        match error {
            EngineError::Execution(message) => Self::Execution(message),
            other => Self::Execution(other.to_string()),
        }
    }
}

pub type EngineResult<T, E> = Result<T, EngineError<E>>;
