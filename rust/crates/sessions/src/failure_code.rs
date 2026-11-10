use crate::SessionsError;
use agentkit_store::StoreError;

impl SessionsError {
    /// Stable failure-event projection for a session-owned failure.
    pub fn failure_event_code(&self) -> String {
        match self {
            Self::InvalidInput(_) | Self::InvalidInputContext(_) => "invalid_input".into(),
            Self::ChatBusy(_) => "chat_busy".into(),
            Self::ChatIdle(_) => "chat_idle".into(),
            Self::InvalidState(_) => "invalid_state".into(),
            Self::InputNotFound(_)
            | Self::TurnNotFound(_)
            | Self::ExecutionNotFound(_)
            | Self::ReceiptNotFound { .. } => "not_found".into(),
            Self::SessionNotFound(_) => "session_not_found".into(),
            Self::AnswerStale(_) => "answer_stale".into(),
            Self::NoExecutorForKind(_) => "no_executor_for_kind".into(),
            Self::SessionContextInvalid(_) => "session_context_invalid".into(),
            Self::InvalidProfile(_) => "invalid_profile".into(),
            Self::InvalidOverrides(_) => "invalid_overrides".into(),
            Self::RunProfileRefused(_) => "run_profile_refused".into(),
            Self::InvalidContentSelectionDetail(_) => "invalid_content_selection".into(),
            Self::InvalidContentSnapshot(_) => "invalid_content_snapshot".into(),
            Self::ReceiptInvalidInput(_) => "invalid_input".into(),
            Self::SessionStore(error) => service_store_code(&error.0).into(),
            Self::AnswerStore(error) if matches!(error.0, StoreError::AnswerStale(_)) => {
                "answer_stale".into()
            }
            Self::AnswerStore(error) => service_store_code(&error.0).into(),
            Self::ReceiptStore(error) => match &error.0 {
                StoreError::ReceiptTransition {
                    reason: agentkit_store::ReceiptTransitionReason::RunActive,
                    ..
                } => "receipt_run_active".into(),
                StoreError::ReceiptTransition {
                    reason: agentkit_store::ReceiptTransitionReason::NotUncertain,
                    ..
                } => "receipt_not_uncertain".into(),
                other => service_store_code(other).into(),
            },
            Self::ProviderCore(error) => error.code.clone(),
            Self::AgentNotFound(id) => format!("agent {}", id.split(':').next().unwrap_or(id)),
            Self::TaskNotFound(id) => format!("task {}", id.split(':').next().unwrap_or(id)),
            Self::InvalidContentAuthorityDetail(_) => "invalid_content_authority".into(),
            Self::ContentAuthorityCallback(_) | Self::Json(_) => "host_error".into(),
            Self::Engine(error) => engine_code(error),
            Self::Store(error) => store_code(error),
            other => other.to_string(),
        }
    }
}

fn service_store_code(error: &StoreError) -> &'static str {
    match error {
        StoreError::NotFound(_) => "not_found",
        StoreError::AnswerConflict(_) => "answer_conflict",
        StoreError::Duplicate(_) => "input_conflict",
        StoreError::ScopeBusy(_) => "chat_busy",
        StoreError::InvalidTransition(_)
        | StoreError::AgentInputTransition(_)
        | StoreError::AnswerStale(_)
        | StoreError::ReceiptTransition { .. }
        | StoreError::LeaseLost(_) => "invalid_state",
        StoreError::InvalidInput(_)
        | StoreError::TaskInput { .. }
        | StoreError::ManagedInput { .. } => "invalid_input",
        _ => "store_failure",
    }
}

fn engine_code(error: &agentkit_engine::EngineError<StoreError>) -> String {
    use agentkit_engine::EngineError as E;
    match error {
        E::Store(error) => store_code(error),
        E::Failure(error) => error.code.clone().unwrap_or_else(|| "host_error".into()),
        E::UnknownKind(_) => "task executor not registered".into(),
        E::NoExecutorForKind(_) => "no executor registered for task kind".into(),
        E::VerificationTimeout => "hook_timeout".into(),
        E::External { failure_code, .. } | E::Context { failure_code, .. } => {
            failure_code.clone().unwrap_or_else(|| "host_error".into())
        }
        E::Worker(_) => "host_error".into(),
        other => other.to_string(),
    }
}
fn store_code(error: &StoreError) -> String {
    match error {
        StoreError::Database(_) => "database error".into(),
        StoreError::Json(_) => "serialization error".into(),
        StoreError::Io(_) => "filesystem error".into(),
        StoreError::NotFound(_) => "record not found".into(),
        StoreError::Duplicate(_) | StoreError::AnswerConflict(_) => "duplicate record".into(),
        StoreError::Conflict(_) => "concurrent modification".into(),
        StoreError::ScopeBusy(_) => "scope busy".into(),
        StoreError::SessionDescriptorChanged(_) => {
            "session descriptor changed while staging settlement".into()
        }
        StoreError::InvalidTransition(_)
        | StoreError::AgentInputTransition(_)
        | StoreError::AnswerStale(_)
        | StoreError::ReceiptTransition { .. } => "invalid transition".into(),
        StoreError::InvalidInput(_)
        | StoreError::TaskInput { .. }
        | StoreError::ManagedInput { .. } => "invalid input".into(),
        StoreError::InvalidImport { .. } => "invalid import".into(),
        StoreError::RecoveryUnreadable(_) => "recovery evidence unreadable".into(),
        StoreError::Internal(_) => "internal store error".into(),
        StoreError::LeaseLost(id) => {
            format!("lease lost for task {}", id.split(':').next().unwrap_or(id))
        }
        other => other.to_string(),
    }
}
