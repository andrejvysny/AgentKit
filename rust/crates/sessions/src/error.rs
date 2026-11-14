//! The session context's error. It is generic over the repository fault
//! (ADR 0024 §3): the session service never names an adapter's error enum, it
//! carries the fault the port reported and projects it through
//! [`RepositoryFault`] predicates. `NeverFault` is the default so a validator
//! or a faultless port reports the same error type with no fault to name.

use agentkit_engine::{NeverFault, RepositoryFault};

#[derive(Debug, thiserror::Error)]
pub enum SessionsError<E: RepositoryFault = NeverFault> {
    #[error("{0}")]
    Engine(#[from] agentkit_engine::EngineError<E>),
    #[error("{0}")]
    Store(#[source] E),
    #[error("{0}")]
    SessionStore(#[source] SessionStoreError<E>),
    #[error("{0}")]
    AnswerStore(#[source] AnswerStoreError<E>),
    #[error("{0}")]
    ReceiptStore(#[source] ReceiptStoreError<E>),
    #[error("execution cancelled")]
    Cancelled,
    #[error("invalid_input: {0}")]
    InvalidInput(String),
    #[error("invalid_input: {0}")]
    InvalidInputContext(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("chat_busy: {0}")]
    ChatBusy(String),
    #[error("chat_idle: {0}")]
    ChatIdle(String),
    #[error("invalid_state: {0}")]
    InvalidState(String),
    /// The same input identity presented again with different bytes: the
    /// adapter's idempotency rule refused it. A distinct variant rather than
    /// a borrowed adapter fault, because the refusal is the session context's
    /// own rule (ADR 0017 part 1), not the store's.
    #[error("input_conflict: {0}")]
    InputConflict(String),
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
    Json(serde_json::Error),
    #[error("invalid_input: {0}")]
    ReceiptInvalidInput(String),
    #[error("not_found: execution {execution_id} not found in chat {chat_id}.")]
    ReceiptNotFound {
        execution_id: String,
        chat_id: String,
    },
}

pub type SessionsResult<T, E = NeverFault> = Result<T, SessionsError<E>>;

impl<E: RepositoryFault> SessionsError<E> {
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

/// A session-aggregate fault reported by the port, rendered with the session
/// context's own service codes.
#[derive(Debug)]
pub struct SessionStoreError<E>(pub E);

impl<E: RepositoryFault> std::fmt::Display for SessionStoreError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", SessionStoreErrorDisplay(&self.0))
    }
}
impl<E: RepositoryFault> std::error::Error for SessionStoreError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
impl<E: RepositoryFault> From<E> for SessionsError<E> {
    fn from(error: E) -> Self {
        Self::SessionStore(SessionStoreError(error))
    }
}

/// A fault from answering a human's request.
#[derive(Debug)]
pub struct AnswerStoreError<E>(pub E);
impl<E: RepositoryFault> std::fmt::Display for AnswerStoreError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.answer_stale() {
            Some(message) => write!(f, "answer_stale: {message}"),
            None => write!(f, "{}", SessionStoreErrorDisplay(&self.0)),
        }
    }
}
impl<E: RepositoryFault> std::error::Error for AnswerStoreError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
/// A fault from resolving an uncertain execution receipt.
#[derive(Debug)]
pub struct ReceiptStoreError<E>(pub E);
impl<E: RepositoryFault> std::fmt::Display for ReceiptStoreError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(message) = self.0.receipt_not_uncertain() {
            return write!(f, "receipt_not_uncertain: {message}");
        }
        if let Some(message) = self.0.receipt_run_active() {
            return write!(f, "receipt_run_active: {message}");
        }
        write!(f, "{}", SessionStoreErrorDisplay(&self.0))
    }
}
impl<E: RepositoryFault> std::error::Error for ReceiptStoreError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
struct SessionStoreErrorDisplay<'a, E>(&'a E);
/// The service code and message of a repository fault, read through the
/// port's predicates (ADR 0024 §3) rather than an adapter's enum.
fn fault_service_parts<E: RepositoryFault>(error: &E) -> (&'static str, String) {
    if let Some(message) = error.not_found() {
        return ("not_found", message.to_owned());
    }
    if let Some(message) = error.answer_conflict() {
        return ("answer_conflict", message.to_owned());
    }
    if let Some(message) = error.input_conflict() {
        return ("input_conflict", message.to_owned());
    }
    if let Some(scope) = error.scope_busy() {
        return ("chat_busy", format!("Chat {scope} is busy."));
    }
    if let Some(message) = error.answer_stale() {
        return ("invalid_state", message.to_owned());
    }
    if let Some(message) = error.invalid_transition() {
        return ("invalid_state", message.to_owned());
    }
    if let Some(task) = error.lease_lost_task() {
        return ("invalid_state", format!("lease lost: {task}"));
    }
    if let Some(message) = error.invalid_input() {
        return ("invalid_input", message.to_owned());
    }
    if error.selected_connector_required() {
        return ("invalid_input", error.to_string());
    }
    ("store_failure", error.to_string())
}
impl<E: RepositoryFault> std::fmt::Display for SessionStoreErrorDisplay<'_, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, message) = fault_service_parts(self.0);
        write!(f, "{code}: {message}")
    }
}
pub struct LegacySessionDebug<'a, E: RepositoryFault>(pub &'a SessionsError<E>);
impl<E: RepositoryFault> std::fmt::Debug for LegacySessionDebug<'_, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (code, message) = match self.0 {
            SessionsError::InvalidInput(message) => ("invalid_input", message.clone()),
            SessionsError::SessionNotFound(message) => ("session_not_found", message.clone()),
            SessionsError::SessionStore(error) => fault_service_parts(&error.0),
            other => ("internal", other.to_string()),
        };
        f.debug_struct("SessionError")
            .field("code", &code)
            .field("message", &message)
            .finish()
    }
}

impl<E: RepositoryFault> SessionsError<E> {
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
            Self::SessionStore(error) => fault_service_classification(&error.0),
            Self::AnswerStore(error) if error.0.answer_stale().is_some() => Unclassified,
            Self::ReceiptStore(error)
                if error.0.receipt_not_uncertain().is_some()
                    || error.0.receipt_run_active().is_some() =>
            {
                Unclassified
            }
            Self::AnswerStore(error) => fault_service_classification(&error.0),
            Self::ReceiptStore(error) => fault_service_classification(&error.0),
            _ => Unclassified,
        }
    }
}

fn fault_service_classification<E: RepositoryFault>(
    error: &E,
) -> agentkit_engine::ContextClassification {
    use agentkit_engine::ContextClassification::{Rejected, Unclassified};
    if error.invalid_transition().is_some()
        || error.answer_stale().is_some()
        || error.receipt_not_uncertain().is_some()
        || error.receipt_run_active().is_some()
        || error.lease_lost_task().is_some()
        || error.invalid_input().is_some()
        || error.task_input_reason().is_some()
        || error.selected_connector_required()
    {
        Rejected
    } else {
        Unclassified
    }
}

impl<E: RepositoryFault> From<SessionsError<E>> for agentkit_engine::EngineError<E> {
    fn from(error: SessionsError<E>) -> Self {
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

impl<E: RepositoryFault> SessionsError<E> {
    /// True when this refusal means "another turn holds the scope": the
    /// session's own busy refusal or the adapter's scope-busy fault. The
    /// caller queues behind the live turn instead of failing.
    pub fn is_scope_busy(&self) -> bool {
        match self {
            Self::ChatBusy(_) => true,
            Self::Store(error) | Self::SessionStore(SessionStoreError(error)) => {
                error.scope_busy().is_some()
            }
            _ => false,
        }
    }

    fn is_external_store(&self) -> bool {
        let error = match self {
            Self::SessionStore(error) => &error.0,
            Self::AnswerStore(error) => &error.0,
            Self::ReceiptStore(error) => &error.0,
            _ => return false,
        };
        error.is_environmental()
    }

    /// The same error carried into a service that names a concrete adapter
    /// fault. Uninhabited on the source side, so the lift loses nothing. (An
    /// inherent method, not `From`: a blanket `From` would make every `?` in
    /// a caller ambiguous.)
    pub fn from_faultless(error: SessionsError<NeverFault>) -> Self {
        match error {
            SessionsError::Engine(error) => {
                Self::Engine(agentkit_engine::EngineError::from_faultless(error))
            }
            SessionsError::Store(fault) => match fault {},
            SessionsError::SessionStore(error) => match error.0 {},
            SessionsError::AnswerStore(error) => match error.0 {},
            SessionsError::ReceiptStore(error) => match error.0 {},
            SessionsError::Cancelled => Self::Cancelled,
            SessionsError::InvalidInput(message) => Self::InvalidInput(message),
            SessionsError::InvalidInputContext(source) => Self::InvalidInputContext(source),
            SessionsError::ChatBusy(message) => Self::ChatBusy(message),
            SessionsError::ChatIdle(message) => Self::ChatIdle(message),
            SessionsError::InvalidState(message) => Self::InvalidState(message),
            SessionsError::InputConflict(message) => Self::InputConflict(message),
            SessionsError::InputNotFound(message) => Self::InputNotFound(message),
            SessionsError::TurnNotFound(message) => Self::TurnNotFound(message),
            SessionsError::ExecutionNotFound(message) => Self::ExecutionNotFound(message),
            SessionsError::SessionNotFound(message) => Self::SessionNotFound(message),
            SessionsError::AnswerStale(message) => Self::AnswerStale(message),
            SessionsError::NoExecutorForKind(message) => Self::NoExecutorForKind(message),
            SessionsError::SessionContextInvalid(message) => Self::SessionContextInvalid(message),
            SessionsError::ProviderCancelled => Self::ProviderCancelled,
            SessionsError::ProviderAuthorityUnavailable => Self::ProviderAuthorityUnavailable,
            SessionsError::InvalidProviderId => Self::InvalidProviderId,
            SessionsError::ConnectionChanged => Self::ConnectionChanged,
            SessionsError::InvalidProviderSnapshot => Self::InvalidProviderSnapshot,
            SessionsError::UnsupportedModel => Self::UnsupportedModel,
            SessionsError::UnknownProvider => Self::UnknownProvider,
            SessionsError::UnsupportedCapability => Self::UnsupportedCapability,
            SessionsError::ProviderCore(error) => Self::ProviderCore(error),
            SessionsError::InvalidProfile(message) => Self::InvalidProfile(message),
            SessionsError::InvalidOverrides(message) => Self::InvalidOverrides(message),
            SessionsError::RunProfileRefused(message) => Self::RunProfileRefused(message),
            SessionsError::AgentNotFound(message) => Self::AgentNotFound(message),
            SessionsError::TaskNotFound(message) => Self::TaskNotFound(message),
            SessionsError::InvalidContentSelectionDetail(message) => {
                Self::InvalidContentSelectionDetail(message)
            }
            SessionsError::InvalidContentSelection => Self::InvalidContentSelection,
            SessionsError::InvalidContentAuthority => Self::InvalidContentAuthority,
            SessionsError::InvalidContentAuthorityDetail(error) => {
                Self::InvalidContentAuthorityDetail(error)
            }
            SessionsError::ContentAuthorityCallback(message) => {
                Self::ContentAuthorityCallback(message)
            }
            SessionsError::ContentAuthorityTimeout => Self::ContentAuthorityTimeout,
            SessionsError::InvalidContentMessage => Self::InvalidContentMessage,
            SessionsError::InvalidContentBlock => Self::InvalidContentBlock,
            SessionsError::ContentSnapshotRequired => Self::ContentSnapshotRequired,
            SessionsError::ContentAuthorityRevoked => Self::ContentAuthorityRevoked,
            SessionsError::ContentAuthorityChanged => Self::ContentAuthorityChanged,
            SessionsError::InvalidContentSnapshot(message) => Self::InvalidContentSnapshot(message),
            SessionsError::ContentSnapshotDigestMismatch => Self::ContentSnapshotDigestMismatch,
            SessionsError::Json(error) => Self::Json(error),
            SessionsError::ReceiptInvalidInput(message) => Self::ReceiptInvalidInput(message),
            SessionsError::ReceiptNotFound {
                execution_id,
                chat_id,
            } => Self::ReceiptNotFound {
                execution_id,
                chat_id,
            },
        }
    }
}
