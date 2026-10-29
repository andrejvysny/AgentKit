//! Durable storage. Every database mutation runs on one bounded, dedicated worker.
mod actor;
mod agent_inputs;
mod conversations;
mod data;
mod execution_receipts;
mod models;
mod proposals;
mod schema;
mod session_ops;
mod settings_commit;
mod tasks;
mod timestamp;
mod util;

pub use actor::SqliteStore;
pub use agent_inputs::*;
pub use conversations::*;
pub use data::*;
pub use execution_receipts::*;
pub use models::*;
pub use proposals::*;
pub use schema::SCHEMA_VERSION;
pub use session_ops::*;
pub use settings_commit::{SettingWrite, SettingsCommit};
pub use timestamp::{ConversationTimestamp, TimestampOutOfRange};

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
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid import: {reason}")]
    InvalidImport { reason: String },
    #[error("recovery evidence unreadable: {0}")]
    RecoveryUnreadable(String),
    #[error("store worker closed")]
    Closed,
}

pub type Result<T> = std::result::Result<T, StoreError>;
