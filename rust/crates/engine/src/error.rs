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

/// The fault type of a port that reports no adapter failure: a port that
/// drives no database has no adapter fault to name. Uninhabited, so every
/// [`RepositoryFault`] predicate is unreachable by construction.
#[derive(Debug)]
pub enum NeverFault {}

impl std::fmt::Display for NeverFault {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

impl std::error::Error for NeverFault {}

impl crate::RepositoryFault for NeverFault {
    fn is_transient(&self) -> bool {
        match *self {}
    }
    fn is_duplicate(&self) -> bool {
        match *self {}
    }
    fn is_lease_lost(&self) -> bool {
        match *self {}
    }
    fn is_invalid_transition(&self) -> bool {
        match *self {}
    }
    fn is_not_found(&self) -> bool {
        match *self {}
    }
    fn invalid_input(&self) -> Option<&str> {
        match *self {}
    }
    fn task_input_reason(&self) -> Option<crate::TaskInputReason> {
        match *self {}
    }
    fn session_conflict(&self) -> Option<&str> {
        match *self {}
    }
}

/// A refusal that names no repository, carried into the fault type of the
/// port its caller drives. The fault-carrying variant is uninhabited on the
/// source side, so the lift loses nothing. (An inherent method, not `From`:
/// a blanket `From` would make every `?` in the engine ambiguous.)
impl<E: std::error::Error> EngineError<E> {
    pub fn from_faultless(error: EngineError<NeverFault>) -> Self {
        match error {
            EngineError::Failure(failure) => Self::Failure(failure),
            EngineError::Store(fault) => match fault {},
            EngineError::UnknownKind(kind) => Self::UnknownKind(kind),
            EngineError::NoExecutorForKind(kind) => Self::NoExecutorForKind(kind),
            EngineError::Cancelled => Self::Cancelled,
            EngineError::InvalidTimestamp => Self::InvalidTimestamp,
            EngineError::InvalidHostLimits => Self::InvalidHostLimits,
            EngineError::InvalidRetryBackoff => Self::InvalidRetryBackoff,
            EngineError::InvalidRetryJitter => Self::InvalidRetryJitter,
            EngineError::RetryDeadlineOverflow => Self::RetryDeadlineOverflow,
            EngineError::VerificationTimeout => Self::VerificationTimeout,
            EngineError::TaskNotFound => Self::TaskNotFound,
            EngineError::Worker(error) => Self::Worker(error),
            EngineError::External {
                source,
                failure_code,
            } => Self::External {
                source,
                failure_code,
            },
            EngineError::Context {
                source,
                classification,
                failure_code,
            } => Self::Context {
                source,
                classification,
                failure_code,
            },
        }
    }
}
