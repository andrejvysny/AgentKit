use agentkit_store::StoreError;

mod projection;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("{0}")]
    Engine(#[source] agentkit_engine::EngineError<StoreError>),
    #[error("{0}")]
    Sessions(#[source] agentkit_sessions::SessionsError<StoreError>),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("{0}")]
    Core(#[from] agentkit_core::CoreError),
    #[error("execution cancelled")]
    Cancelled,
    #[error("hook_timeout: {0}")]
    HookTimeout(String),
    #[error("{0}")]
    External(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("{0}")]
    Failure(#[from] agentkit_engine::ExecutionFailure),
    #[error("invalid_reasoning_effort: {0}")]
    InvalidReasoningEffort(#[source] serde_json::Error),
    #[error("Cannot read durable execution receipts; mutations remain blocked. ({0})")]
    ReadReceipts(#[source] StoreError),
    #[error("{0} required")]
    RequiredField(String),
    #[error("invalid_budget: {0}")]
    InvalidBudget(String),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Worker(#[from] tokio::task::JoinError),
    #[error("Cannot resolve workspace root: {0}")]
    ResolveWorkspaceRoot(#[source] std::io::Error),
    #[error("Cannot open workspace root: {0}")]
    OpenWorkspaceRoot(#[source] std::io::Error),
    #[error("Cannot inspect instruction file: {0}")]
    InspectInstructionFile(#[source] std::io::Error),
    #[error("Cannot open instruction file: {0}")]
    OpenInstructionFile(#[source] std::io::Error),
    #[error("Cannot stat instruction file: {0}")]
    StatInstructionFile(#[source] std::io::Error),
    #[error("Cannot identify instruction file: {0}")]
    IdentifyInstructionFile(#[source] std::io::Error),
    #[error("Cannot read instruction file: {0}")]
    ReadInstructionFile(#[source] std::io::Error),
    #[error("Instruction file name contains a NUL byte.")]
    InstructionFileNameContainsANulByte,
    #[error("Instruction reader stopped.")]
    InstructionReaderStopped,
    #[error("Workspace root contains a NUL byte.")]
    WorkspaceRootContainsANulByte,
    #[error("assistant placeholder required")]
    AssistantPlaceholderRequired,
    #[error("chat scope mismatch")]
    ChatScopeMismatch,
    #[error("compaction interrupted")]
    CompactionInterrupted,
    #[error("compaction task disappeared")]
    CompactionTaskDisappeared,
    #[error("context snapshot failed")]
    ContextSnapshotFailed,
    #[error("invalid attachment reference")]
    InvalidAttachmentReference,
    #[error("invalid context binding")]
    InvalidContextBinding,
    #[error("invalid context provider result")]
    InvalidContextProviderResult,
    #[error("invalid resolved attachment")]
    InvalidResolvedAttachment,
    #[error("invalid runtime timestamp")]
    InvalidRuntimeTimestamp,
    #[error("invalid verification status")]
    InvalidVerificationStatus,
    #[error("invalid_context_window_snapshot")]
    InvalidContextWindowSnapshot,
    #[error("invalid_max_output_tokens")]
    InvalidMaxOutputTokens,
    #[error("{0}")]
    Governance(#[from] agentkit_governance::GovernanceError),
    #[error("invalid_provider_connection")]
    InvalidProviderConnection,
    #[error("invalid_provider_context_window")]
    InvalidProviderContextWindow,
    #[error("profile_snapshot_required")]
    ProfileSnapshotRequired,
    #[error("provider_binding_mismatch")]
    ProviderBindingMismatch,
    #[error("provider_snapshot_required")]
    ProviderSnapshotRequired,
    #[error("receipt snapshot failed")]
    ReceiptSnapshotFailed,
    #[error("session disappeared")]
    SessionDisappeared,
    #[error("session not found")]
    SessionNotFound,
    #[error("session_context_invalid")]
    SessionContextInvalid,
    #[error("summary source hash failed")]
    SummarySourceHashFailed,
    #[error("summary source id missing")]
    SummarySourceIdMissing,
    #[error("Instruction file changed during open.")]
    InstructionFileChangedDuringOpen,
    #[error("Instruction file is a symlink.")]
    InstructionFileIsASymlink,
    #[error("Instruction file is not a regular file.")]
    InstructionFileIsNotARegularFile,
    #[error("Instruction file name must be a plain file name.")]
    InstructionFileNameMustBeAPlainFileName,
    #[error("Project instructions are not read on this platform.")]
    ProjectInstructionsAreNotReadOnThisPlatform,
    #[error("actor_allowance_requires_exact_scope_and_payload")]
    ActorAllowanceRequiresExactScopeAndPayload,
    #[error("context changed during compaction")]
    ContextChangedDuringCompaction,
    #[error("execution_budget_exhausted: retries")]
    ExecutionBudgetExhaustedRetries,
    #[error("provider_continuation_branch_mismatch")]
    ProviderContinuationBranchMismatch,
    #[error("provider_continuation_scope_mismatch")]
    ProviderContinuationScopeMismatch,
    #[error("provider_snapshot_model_mismatch")]
    ProviderSnapshotModelMismatch,
    #[error("session_submission_required: use AgentSessionService for session turns")]
    SessionSubmissionRequired,
    #[error("write_tool_requires_write_effect_and_trusted_actor")]
    WriteToolRequiresWriteEffectAndTrustedActor,
}

pub type ChatResult<T> = Result<T, ChatError>;

impl From<ChatError> for agentkit_engine::EngineError<StoreError> {
    fn from(error: ChatError) -> Self {
        use agentkit_engine::EngineError;
        match error {
            ChatError::Engine(error) => error,
            ChatError::Store(error) => EngineError::Store(error),
            ChatError::Cancelled => EngineError::Cancelled,
            ChatError::Failure(error) => EngineError::Failure(error),
            ChatError::Sessions(error) => error.into(),
            ChatError::Core(error) => EngineError::External {
                failure_code: Some(error.code.clone()),
                source: Box::new(error),
            },
            ChatError::External(error) => EngineError::External {
                source: error,
                failure_code: None,
            },
            ChatError::Worker(error) => EngineError::Worker(error),
            ChatError::Governance(agentkit_governance::GovernanceError::Store(error)) => {
                EngineError::Store(error)
            }
            ChatError::Governance(agentkit_governance::GovernanceError::Core(error)) => {
                EngineError::External {
                    failure_code: Some(error.code.clone()),
                    source: Box::new(error),
                }
            }
            ChatError::Json(error) => EngineError::External {
                source: Box::new(error),
                failure_code: None,
            },
            error @ (ChatError::ReadReceipts(_)
            | ChatError::ResolveWorkspaceRoot(_)
            | ChatError::OpenWorkspaceRoot(_)
            | ChatError::InspectInstructionFile(_)
            | ChatError::OpenInstructionFile(_)
            | ChatError::StatInstructionFile(_)
            | ChatError::IdentifyInstructionFile(_)
            | ChatError::ReadInstructionFile(_)) => EngineError::External {
                failure_code: Some(error.failure_event_code()),
                source: Box::new(error),
            },
            other => EngineError::Context {
                classification: other.context_classification(),
                failure_code: Some(other.failure_event_code()),
                source: Box::new(other),
            },
        }
    }
}

impl ChatError {
    fn context_classification(&self) -> agentkit_engine::ContextClassification {
        use agentkit_engine::ContextClassification::{Rejected, Unclassified};
        match self {
            Self::Governance(error) => error.context_classification(),
            Self::InvalidReasoningEffort(_)
            | Self::InvalidBudget(_)
            | Self::InvalidAttachmentReference
            | Self::InvalidContextBinding
            | Self::InvalidContextProviderResult
            | Self::InvalidResolvedAttachment
            | Self::InvalidRuntimeTimestamp
            | Self::InvalidVerificationStatus
            | Self::InvalidContextWindowSnapshot
            | Self::InvalidMaxOutputTokens
            | Self::InvalidProviderConnection
            | Self::InvalidProviderContextWindow
            | Self::SessionContextInvalid => Rejected,
            _ => Unclassified,
        }
    }
}

impl From<agentkit_engine::EngineError<StoreError>> for ChatError {
    fn from(error: agentkit_engine::EngineError<StoreError>) -> Self {
        match error {
            agentkit_engine::EngineError::Cancelled => Self::Cancelled,
            agentkit_engine::EngineError::Failure(error) => Self::Failure(error),
            agentkit_engine::EngineError::Store(error) => Self::Store(error),
            other => Self::Engine(other),
        }
    }
}
impl From<agentkit_sessions::SessionsError<StoreError>> for ChatError {
    fn from(error: agentkit_sessions::SessionsError<StoreError>) -> Self {
        match error {
            agentkit_sessions::SessionsError::Engine(error) => error.into(),
            agentkit_sessions::SessionsError::Cancelled => Self::Cancelled,
            other => Self::Sessions(other),
        }
    }
}

/// The same for a refusal that reports no adapter fault (a validator, a
/// faultless port): lifted into the fault the session service reports, which
/// loses nothing because the fault-carrying variants are uninhabited there.
impl From<agentkit_sessions::SessionsError> for ChatError {
    fn from(error: agentkit_sessions::SessionsError) -> Self {
        Self::from(agentkit_sessions::SessionsError::<StoreError>::from_faultless(error))
    }
}

#[cfg(test)]
mod tests;

impl ChatError {
    pub(crate) fn failure_event_code(&self) -> String {
        projection::code(self)
    }
}
