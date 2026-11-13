use agentkit_store::StoreError;

#[derive(Debug, thiserror::Error)]
pub enum SessionsError {
    #[error("{0}")]
    Engine(#[from] agentkit_engine::EngineError<StoreError>),
    #[error("{0}")]
    Store(#[source] StoreError),
    #[error("{0}")]
    SessionStore(#[source] SessionStoreError),
    #[error("{0}")]
    AnswerStore(#[source] AnswerStoreError),
    #[error("{0}")]
    ReceiptStore(#[source] ReceiptStoreError),
    #[error("execution cancelled")]
    Cancelled,
    #[error("invalid_input: {0}")]
    InvalidInput(String),
    #[error("invalid_input: {0}")]
    InvalidInputContext(#[source] Box<SessionsError>),
    #[error("chat_busy: {0}")]
    ChatBusy(String),
    #[error("chat_idle: {0}")]
    ChatIdle(String),
    #[error("invalid_state: {0}")]
    InvalidState(String),
    #[error("not_found: {0}")]
    InputNotFound(String),
    #[error("not_found: {0}")]
    TurnNotFound(String),
    #[error("not_found: {0}")]
    ExecutionNotFound(String),
    #[error("session_not_found: {0}")]
    SessionNotFound(String),
    #[error("answer_stale: {0}")]
    AnswerStale(String),
    #[error("no_executor_for_kind: {0}")]
    NoExecutorForKind(String),
    #[error("session_context_invalid: {0}")]
    SessionContextInvalid(String),
    #[error("cancelled")]
    ProviderCancelled,
    #[error("provider_authority_unavailable")]
    ProviderAuthorityUnavailable,
    #[error("invalid_provider_id")]
    InvalidProviderId,
    #[error("connection_changed")]
    ConnectionChanged,
    #[error("invalid_provider_snapshot")]
    InvalidProviderSnapshot,
    #[error("unsupported_model")]
    UnsupportedModel,
    #[error("unknown_provider")]
    UnknownProvider,
    #[error("unsupported_capability")]
    UnsupportedCapability,
    #[error("{}", .0.code)]
    ProviderCore(#[source] agentkit_core::CoreError),
    #[error("invalid_profile: {0}")]
    InvalidProfile(String),
    #[error("invalid_overrides: {0}")]
    InvalidOverrides(String),
    #[error("run_profile_refused:{0}")]
    RunProfileRefused(String),
    #[error("agent {0}")]
    AgentNotFound(String),
    #[error("task {0}")]
    TaskNotFound(String),
    #[error("invalid_content_selection: {0}")]
    InvalidContentSelectionDetail(String),
    #[error("invalid_content_selection")]
    InvalidContentSelection,
    #[error("invalid_content_authority")]
    InvalidContentAuthority,
    #[error("invalid_content_authority: {0}")]
    InvalidContentAuthorityDetail(#[source] serde_json::Error),
    #[error("{0}")]
    ContentAuthorityCallback(String),
    #[error("content_authority_timeout")]
    ContentAuthorityTimeout,
    #[error("invalid_content_message")]
    InvalidContentMessage,
    #[error("invalid_content_block")]
    InvalidContentBlock,
    #[error("content_snapshot_required")]
    ContentSnapshotRequired,
    #[error("content_authority_revoked")]
    ContentAuthorityRevoked,
    #[error("content_authority_changed")]
    ContentAuthorityChanged,
    #[error("invalid_content_snapshot: {0}")]
    InvalidContentSnapshot(String),
    #[error("content_snapshot_digest_mismatch")]
    ContentSnapshotDigestMismatch,
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid_input: {0}")]
    ReceiptInvalidInput(String),
    #[error("not_found: execution {execution_id} not found in chat {chat_id}.")]
    ReceiptNotFound {
        execution_id: String,
        chat_id: String,
    },
}

pub type SessionsResult<T> = Result<T, SessionsError>;

impl SessionsError {
    pub(crate) fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
    pub(crate) fn chat_busy(session_id: &str) -> Self {
        Self::ChatBusy(format!("Chat {session_id} is busy."))
    }
    pub(crate) fn chat_idle(message: impl Into<String>) -> Self {
        Self::ChatIdle(message.into())
    }
    pub(crate) fn invalid_state(message: impl Into<String>) -> Self {
        Self::InvalidState(message.into())
    }
    pub(crate) fn session_not_found(message: impl Into<String>) -> Self {
        Self::SessionNotFound(message.into())
    }
    pub(crate) fn answer_stale(message: impl Into<String>) -> Self {
        Self::AnswerStale(message.into())
    }
    pub(crate) fn no_executor_for_kind(message: impl Into<String>) -> Self {
        Self::NoExecutorForKind(message.into())
    }
    pub(crate) fn session_context_invalid(message: impl Into<String>) -> Self {
        Self::SessionContextInvalid(message.into())
    }
}

#[derive(Debug)]
pub struct SessionStoreError(pub StoreError);

impl std::fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", SessionStoreErrorDisplay(&self.0))
    }
}
impl std::error::Error for SessionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
impl From<StoreError> for SessionsError {
    fn from(error: StoreError) -> Self {
        Self::SessionStore(SessionStoreError(error))
    }
}

#[derive(Debug)]
pub struct AnswerStoreError(pub StoreError);
impl std::fmt::Display for AnswerStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            StoreError::AnswerStale(message) => write!(f, "answer_stale: {message}"),
            other => write!(f, "{}", SessionStoreErrorDisplay(other)),
        }
    }
}
impl std::error::Error for AnswerStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
#[derive(Debug)]
pub struct ReceiptStoreError(pub StoreError);
impl std::fmt::Display for ReceiptStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            StoreError::ReceiptTransition {
                reason: agentkit_store::ReceiptTransitionReason::NotUncertain,
                message,
            } => write!(f, "receipt_not_uncertain: {message}"),
            StoreError::ReceiptTransition {
                reason: agentkit_store::ReceiptTransitionReason::RunActive,
                message,
            } => write!(f, "receipt_run_active: {message}"),
            other => write!(f, "{}", SessionStoreErrorDisplay(other)),
        }
    }
}
impl std::error::Error for ReceiptStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
struct SessionStoreErrorDisplay<'a>(&'a StoreError);
fn store_service_parts(error: &StoreError) -> (&'static str, String) {
    use StoreError as E;
    match error {
        E::NotFound(message) => ("not_found", message.clone()),
        E::AnswerConflict(message) => ("answer_conflict", message.clone()),
        E::Duplicate(message) => ("input_conflict", message.clone()),
        E::ScopeBusy(scope) => ("chat_busy", format!("Chat {scope} is busy.")),
        E::InvalidTransition(message)
        | E::AgentInputTransition(message)
        | E::AnswerStale(message)
        | E::ReceiptTransition { message, .. } => ("invalid_state", message.clone()),
        E::LeaseLost(task) => ("invalid_state", format!("lease lost: {task}")),
        E::InvalidInput(message)
        | E::TaskInput { message, .. }
        | E::ManagedInput { message, .. } => ("invalid_input", message.clone()),
        other => ("store_failure", other.to_string()),
    }
}
impl std::fmt::Display for SessionStoreErrorDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, message) = store_service_parts(self.0);
        write!(f, "{code}: {message}")
    }
}
pub(crate) struct LegacySessionDebug<'a>(pub &'a SessionsError);
impl std::fmt::Debug for LegacySessionDebug<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, message) = match self.0 {
            SessionsError::InvalidInput(message) => ("invalid_input", message.clone()),
            SessionsError::SessionNotFound(message) => ("session_not_found", message.clone()),
            SessionsError::SessionStore(error) => store_service_parts(&error.0),
            other => ("internal", other.to_string()),
        };
        f.debug_struct("SessionError")
            .field("code", &code)
            .field("message", &message)
            .finish()
    }
}

impl SessionsError {
    pub fn context_classification(&self) -> agentkit_engine::ContextClassification {
        use agentkit_engine::ContextClassification::{Rejected, Unclassified};
        match self {
            Self::InvalidInput(_)
            | Self::InvalidInputContext(_)
            | Self::InvalidState(_)
            | Self::InvalidProfile(_)
            | Self::InvalidOverrides(_)
            | Self::InvalidContentSelectionDetail(_)
            | Self::InvalidContentSelection
            | Self::InvalidProviderId
            | Self::InvalidProviderSnapshot
            | Self::InvalidContentAuthorityDetail(_)
            | Self::InvalidContentAuthority
            | Self::InvalidContentMessage
            | Self::InvalidContentBlock
            | Self::InvalidContentSnapshot(_)
            | Self::SessionContextInvalid(_)
            | Self::ReceiptInvalidInput(_) => Rejected,
            Self::SessionStore(error) => store_service_classification(&error.0),
            Self::AnswerStore(error) if matches!(error.0, StoreError::AnswerStale(_)) => {
                Unclassified
            }
            Self::ReceiptStore(error)
                if matches!(error.0, StoreError::ReceiptTransition { .. }) =>
            {
                Unclassified
            }
            Self::AnswerStore(error) => store_service_classification(&error.0),
            Self::ReceiptStore(error) => store_service_classification(&error.0),
            _ => Unclassified,
        }
    }
}

fn store_service_classification(error: &StoreError) -> agentkit_engine::ContextClassification {
    use agentkit_engine::ContextClassification::{Rejected, Unclassified};
    match error {
        StoreError::InvalidTransition(_)
        | StoreError::AgentInputTransition(_)
        | StoreError::AnswerStale(_)
        | StoreError::ReceiptTransition { .. }
        | StoreError::LeaseLost(_)
        | StoreError::InvalidInput(_)
        | StoreError::TaskInput { .. }
        | StoreError::ManagedInput { .. } => Rejected,
        _ => Unclassified,
    }
}

impl From<SessionsError> for agentkit_engine::EngineError<StoreError> {
    fn from(error: SessionsError) -> Self {
        use agentkit_engine::EngineError;
        match error {
            SessionsError::Engine(error) => error,
            SessionsError::Store(error) => EngineError::Store(error),
            SessionsError::Cancelled => EngineError::Cancelled,
            error @ (SessionsError::SessionStore(_)
            | SessionsError::AnswerStore(_)
            | SessionsError::ReceiptStore(_))
                if error.is_external_store() =>
            {
                EngineError::External {
                    source: Box::new(error),
                    failure_code: Some("store_failure".into()),
                }
            }
            error @ SessionsError::ProviderCore(_) => EngineError::External {
                failure_code: Some(error.failure_event_code()),
                source: Box::new(error),
            },
            error @ (SessionsError::ContentAuthorityCallback(_) | SessionsError::Json(_)) => {
                EngineError::External {
                    source: Box::new(error),
                    failure_code: None,
                }
            }
            other => {
                let classification = other.context_classification();
                EngineError::Context {
                    failure_code: Some(other.failure_event_code()),
                    source: Box::new(other),
                    classification,
                }
            }
        }
    }
}

impl SessionsError {
    fn is_external_store(&self) -> bool {
        let error = match self {
            Self::SessionStore(error) => &error.0,
            Self::AnswerStore(error) => &error.0,
            Self::ReceiptStore(error) => &error.0,
            _ => return false,
        };
        matches!(
            error,
            StoreError::Database(_) | StoreError::Io(_) | StoreError::Json(_)
        )
    }
}
