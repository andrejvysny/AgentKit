use super::ChatError;
use agentkit_engine::EngineError;
use agentkit_store::StoreError;

pub(super) fn code(error: &ChatError) -> String {
    match error {
        ChatError::Engine(error) => engine_code(error),
        ChatError::Sessions(error) => error.failure_event_code(),
        ChatError::Store(error) => store_code(error),
        ChatError::Core(error) => error.code.clone(),
        ChatError::Failure(error) => error.code.clone().unwrap_or_else(|| "host_error".into()),
        ChatError::External(_) | ChatError::Json(_) | ChatError::Worker(_) => "host_error".into(),
        ChatError::ReadReceipts(error) => receipt_read_code(error),
        ChatError::ResolveWorkspaceRoot(_) => "Cannot resolve workspace root".into(),
        ChatError::OpenWorkspaceRoot(_) => "Cannot open workspace root".into(),
        ChatError::InspectInstructionFile(_) => "Cannot inspect instruction file".into(),
        ChatError::OpenInstructionFile(_) => "Cannot open instruction file".into(),
        ChatError::StatInstructionFile(_) => "Cannot stat instruction file".into(),
        ChatError::IdentifyInstructionFile(_) => "Cannot identify instruction file".into(),
        ChatError::ReadInstructionFile(_) => "Cannot read instruction file".into(),
        ChatError::HookTimeout(_) => "hook_timeout".into(),
        ChatError::InvalidReasoningEffort(_) => "invalid_reasoning_effort".into(),
        ChatError::InvalidBudget(_) => "invalid_budget".into(),
        ChatError::ExecutionBudgetExhaustedRetries => "execution_budget_exhausted".into(),
        ChatError::SessionSubmissionRequired => "session_submission_required".into(),
        other => other.to_string(),
    }
}

fn engine_code(error: &EngineError<StoreError>) -> String {
    match error {
        EngineError::Store(error) => store_code(error),
        EngineError::Failure(error) => error.code.clone().unwrap_or_else(|| "host_error".into()),
        EngineError::UnknownKind(_) => "task executor not registered".into(),
        EngineError::NoExecutorForKind(_) => "no executor registered for task kind".into(),
        EngineError::VerificationTimeout => "hook_timeout".into(),
        EngineError::External { failure_code, .. } | EngineError::Context { failure_code, .. } => {
            failure_code.clone().unwrap_or_else(|| "host_error".into())
        }
        EngineError::Worker(_) => "host_error".into(),
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

fn receipt_read_code(error: &StoreError) -> String {
    let prefix = "Cannot read durable execution receipts; mutations remain blocked. (";
    let fragment = store_code(error);
    let has_colon = match error {
        StoreError::AlreadyOwned
        | StoreError::ForeignDatabase
        | StoreError::UnsupportedVersion(_)
        | StoreError::Closed
        | StoreError::TransactionGateTimeout
        | StoreError::TransactionScopeExpired => false,
        StoreError::LeaseLost(id) => id.contains(':'),
        _ => true,
    };
    format!("{prefix}{fragment}{}", if has_colon { "" } else { ")" })
}
