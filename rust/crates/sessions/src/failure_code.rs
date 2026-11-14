//! The stable failure-event projection of a session error. Every code here
//! comes from the port's [`RepositoryFault`] predicates (ADR 0024 §3), never
//! from an adapter's enum, so an adapter that organises its faults differently
//! still reports the same codes — the conformance suite pins them.

use crate::SessionsError;
use agentkit_engine::RepositoryFault;

impl<E: RepositoryFault> SessionsError<E> {
    /// Stable failure-event projection for a session-owned failure.
    pub fn failure_event_code(&self) -> String {
        match self {
            Self::InvalidInput(_) | Self::InvalidInputContext(_) => "invalid_input".into(),
            Self::ChatBusy(_) => "chat_busy".into(),
            Self::ChatIdle(_) => "chat_idle".into(),
            Self::InvalidState(_) => "invalid_state".into(),
            Self::InputConflict(_) => "input_conflict".into(),
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
            Self::AnswerStore(error) if error.0.answer_stale().is_some() => "answer_stale".into(),
            Self::AnswerStore(error) => service_store_code(&error.0).into(),
            Self::ReceiptStore(error) => {
                if error.0.receipt_run_active().is_some() {
                    "receipt_run_active".into()
                } else if error.0.receipt_not_uncertain().is_some() {
                    "receipt_not_uncertain".into()
                } else {
                    service_store_code(&error.0).into()
                }
            }
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

fn service_store_code<E: RepositoryFault>(error: &E) -> &'static str {
    if error.not_found().is_some() {
        "not_found"
    } else if error.answer_conflict().is_some() {
        "answer_conflict"
    } else if error.input_conflict().is_some() {
        "input_conflict"
    } else if error.scope_busy().is_some() {
        "chat_busy"
    } else if error.answer_stale().is_some()
        || error.invalid_transition().is_some()
        || error.receipt_not_uncertain().is_some()
        || error.receipt_run_active().is_some()
        || error.lease_lost_task().is_some()
    {
        "invalid_state"
    } else if error.invalid_input().is_some()
        || error.task_input_reason().is_some()
        || error.selected_connector_required()
    {
        "invalid_input"
    } else {
        "store_failure"
    }
}

fn engine_code<E: RepositoryFault>(error: &agentkit_engine::EngineError<E>) -> String {
    use agentkit_engine::EngineError as E_;
    match error {
        E_::Store(error) => store_code(error),
        E_::Failure(error) => error.code.clone().unwrap_or_else(|| "host_error".into()),
        E_::UnknownKind(_) => "task executor not registered".into(),
        E_::NoExecutorForKind(_) => "no executor registered for task kind".into(),
        E_::VerificationTimeout => "hook_timeout".into(),
        E_::External { failure_code, .. } | E_::Context { failure_code, .. } => {
            failure_code.clone().unwrap_or_else(|| "host_error".into())
        }
        E_::Worker(_) => "host_error".into(),
        other => other.to_string(),
    }
}

/// The human-readable store fault, classified through the port's predicates.
fn store_code<E: RepositoryFault>(error: &E) -> String {
    if let Some(task) = error.lease_lost_task() {
        return format!(
            "lease lost for task {}",
            task.split(':').next().unwrap_or(task)
        );
    }
    if error.scope_busy().is_some() {
        return "scope busy".into();
    }
    if error.session_conflict().is_some() {
        return "session descriptor changed while staging settlement".into();
    }
    if error.not_found().is_some() {
        return "record not found".into();
    }
    if error.input_conflict().is_some() || error.answer_conflict().is_some() {
        return "duplicate record".into();
    }
    if error.concurrent_modification().is_some() {
        return "concurrent modification".into();
    }
    if let Some(class) = error.environmental_class() {
        return match class {
            "filesystem" => "filesystem error".into(),
            "serialization" => "serialization error".into(),
            _ => "database error".into(),
        };
    }
    if error.invalid_transition().is_some()
        || error.answer_stale().is_some()
        || error.receipt_not_uncertain().is_some()
        || error.receipt_run_active().is_some()
    {
        return "invalid transition".into();
    }
    if error.invalid_input().is_some()
        || error.task_input_reason().is_some()
        || error.selected_connector_required()
    {
        return "invalid input".into();
    }
    // A fault the port has no predicate for: the adapter's own label, or its
    // `Display` when it has none.
    match error.label() {
        Some(label) => label.to_owned(),
        None => error.to_string(),
    }
}
