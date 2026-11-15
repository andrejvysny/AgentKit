//! The check behind a brief's `memory` references (roadmap 4.1, DEL-02): a
//! parent may hand on only a memory record it can read itself. The memory
//! service lives in the host, so the host supplies the reader.

use async_trait::async_trait;

/// Whether a session may read one memory record.
#[async_trait]
pub trait MemoryRefReader: Send + Sync {
    /// True when `session_id` may read the record named `record_id` (a
    /// hyphenated UUID). A missing, retracted or out-of-scope record is
    /// `false`; the caller never says which.
    async fn can_read(&self, session_id: &str, record_id: &str) -> bool;
}
