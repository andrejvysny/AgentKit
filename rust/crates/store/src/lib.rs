//! Durable storage. Every database mutation runs on one bounded, dedicated worker.
mod actor;
mod advisor;
mod agent_inputs;
mod agents;
mod connector_crypto;
mod context_summaries;
mod conversations;
mod data;
mod deadline;
mod engine_repository;
mod execution_no_start;
mod execution_receipts;
mod execution_resolution;
mod faults;
mod managed_content;
mod proposals;
mod provider_catalog;
mod provider_connections;
mod run_profiles;
mod schema;
mod session_ops;
mod settings_commit;
mod subagents;
mod tasks;
mod teammates;
mod transaction;
#[cfg(feature = "ts-import")]
mod ts_import;
mod util;

pub use actor::{SqliteStore, StoreActorIdentity};
pub use advisor::*;
pub use agent_inputs::*;
pub use agentkit_engine::{
    AttemptRecord, ClaimInput, ClaimedTask, EventRecord, Lease, LeaseFence, MessageInput,
    PinnedRunProfile, RecoveryMode, RetryTask, SettleTask, SubmitTask, TaskBinding, TaskRecord,
    TaskStatus, TaskTransitionPatch,
};
pub use agentkit_kernel::{ConversationTimestamp, TimestampOutOfRange};
pub use agents::*;
pub use connector_crypto::ResealReport;
pub use context_summaries::*;
pub use conversations::*;
pub use data::*;
pub use execution_receipts::*;
pub use execution_resolution::*;
pub use managed_content::*;
pub use proposals::*;
pub use provider_catalog::*;
pub use provider_connections::{
    ProviderContinuityRestore, check_provider_connection, provider_connection_identity,
    register_provider_connection, register_provider_connection_with_continuity,
    register_scoped_provider_connection, restore_provider_continuity,
    revoke_persisted_provider_connection,
};
pub use run_profiles::run_profile_digest;
pub use schema::SCHEMA_VERSION;
pub use session_ops::*;
pub use settings_commit::{SettingWrite, SettingsCommit};
pub use subagents::*;
pub use tasks::{
    MAX_TOOL_TEXT_BYTES, TaskAging, TaskProjection, TaskProjectionApproval, TaskProjectionUsage,
    derive_task_projection, redact_event,
};
pub use teammates::*;
pub use transaction::StoreTransaction;
#[cfg(feature = "ts-import")]
pub use ts_import::{
    ASSISTANT_SETTINGS_KEY, ASSISTANT_SETTINGS_NAMESPACE, ImportOptions, ImportReport,
    MCP_CONFIG_NAMESPACE, MCP_SERVERS_NAMESPACE, SETTINGS_KEY, SETTINGS_NAMESPACE,
};
pub use util::id;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptTransitionReason {
    NotUncertain,
    RunActive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedInputReason {
    SecretReferencesUnsupported,
    InvalidAlias,
    IdentityRequired,
    ImmutableIdentity,
    AliasConflict,
    SelectedConnectorRequired,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("database is already owned by another store")]
    AlreadyOwned,
    #[error("foreign SQLite database; refusing to modify it")]
    ForeignDatabase,
    #[error("unsupported database schema version {0}")]
    UnsupportedVersion(i64),
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("duplicate record: {0}")]
    Duplicate(String),
    #[error("duplicate record: {0}")]
    AnswerConflict(String),
    /// Optimistic concurrency: the record moved out from under the caller's
    /// expected version and the write applied NOTHING. The caller re-reads
    /// and retries against the current version; versions never silently
    /// overwrite each other.
    #[error("concurrent modification: {0}")]
    Conflict(String),
    #[error("scope busy: {0}")]
    ScopeBusy(String),
    /// The settlement committed nothing: the caller may rebuild its plan
    /// against the current descriptor and retry under the same lease.
    #[error("session descriptor changed while staging settlement: {0}")]
    SessionDescriptorChanged(String),
    #[error("lease lost for task {0}")]
    LeaseLost(String),
    #[error("invalid transition: {0}")]
    InvalidTransition(String),
    #[error("invalid transition: {0}")]
    AnswerStale(String),
    #[error("invalid transition: {0}")]
    AgentInputTransition(String),
    #[error("invalid transition: {message}")]
    ReceiptTransition {
        reason: ReceiptTransitionReason,
        message: String,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid input: {message}")]
    TaskInput {
        reason: agentkit_engine::TaskInputReason,
        message: String,
    },
    #[error("invalid input: {message}")]
    ManagedInput {
        reason: ManagedInputReason,
        message: String,
    },
    #[error("invalid import: {reason}")]
    InvalidImport { reason: String },
    #[error("recovery evidence unreadable: {0}")]
    RecoveryUnreadable(String),
    #[error("store worker closed")]
    Closed,
    #[error("transaction_gate_timeout")]
    TransactionGateTimeout,
    #[error("transaction scope expired")]
    TransactionScopeExpired,
    /// A store job panicked; its transaction was rolled back and the worker
    /// kept running.
    #[error("internal store error: {0}")]
    Internal(String),
}

impl From<TimestampOutOfRange> for StoreError {
    fn from(error: TimestampOutOfRange) -> Self {
        Self::InvalidInput(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
