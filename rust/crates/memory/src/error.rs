//! The memory context's error type. Ports are not generic over an adapter
//! fault: adapters map their own errors onto [`MemoryError::Backend`].

use crate::ids::{ModelId, RecordId};
use crate::model::Status;

pub type MemoryResult<T> = Result<T, MemoryError>;

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("memory record {0} not found")]
    NotFound(RecordId),
    /// The record a proposal would replace is no longer current and active;
    /// the proposal stays pending.
    #[error(
        "stale_replacement: memory record {target} was replaced or retracted; proposal {proposal} stays pending"
    )]
    StaleReplacement {
        proposal: RecordId,
        target: RecordId,
    },
    #[error("memory record transition {from:?} -> {to:?} is not allowed")]
    InvalidTransition { from: Status, to: Status },
    #[error("invalid memory input: {0}")]
    InvalidInput(String),
    /// A search must name at least one scope: an empty scope list would
    /// otherwise mean "every scope", a cross-scope leak.
    #[error("memory search needs at least one scope key")]
    EmptyScope,
    #[error("embedding model {0} is unknown to the index")]
    UnknownModel(ModelId),
    /// The transaction's base moved before commit; nothing was applied.
    #[error("memory transaction conflict: {0}")]
    Conflict(String),
    /// A transaction handle came from a different implementation.
    #[error("memory transaction handle does not belong to this implementation")]
    ForeignTransaction,
    #[error("memory backend error: {0}")]
    Backend(String),
}
