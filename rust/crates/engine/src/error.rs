use crate::ExecutionFailure;

#[derive(Clone, Copy, Debug)]
pub enum ContextClassification {
    Rejected,
    Unclassified,
}

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
    #[error("invalid runtime timestamp")]
    InvalidTimestamp,
    #[error("invalid host execution limits")]
    InvalidHostLimits,
    #[error("invalid retry backoff")]
    InvalidRetryBackoff,
    #[error("invalid retry jitter sample")]
    InvalidRetryJitter,
    #[error("retry deadline overflow")]
    RetryDeadlineOverflow,
    #[error("hook_timeout: verify")]
    VerificationTimeout,
    #[error("task not found")]
    TaskNotFound,
    #[error("{0}")]
    Worker(#[source] tokio::task::JoinError),
    #[error("{source}")]
    External {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
        /// Persistence metadata only; never used for retries or transport mapping.
        failure_code: Option<String>,
    },
    #[error("{source}")]
    Context {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
        classification: ContextClassification,
        /// Persistence metadata only; never used for retries or transport mapping.
        failure_code: Option<String>,
    },
}

pub type EngineResult<T, E> = Result<T, EngineError<E>>;
