//! The memory-record check behind a subagent brief's `memory` references
//! (roadmap 4.1, DEL-02): a parent hands on only a record it can read.

use std::sync::Arc;

use agentkit_agents::MemoryRefReader;
use agentkit_memory::{MemoryService, RecordId, ScopeKind, Status};
use async_trait::async_trait;

/// Reads through the runtime's memory service. A record is readable when it
/// is active and current, and sits in the owner's user scope or in the
/// asking session's own agent scope. Project-scope records are not handed on
/// until a session knows its project.
pub(crate) struct MemoryRefAccess {
    memory: Arc<MemoryService>,
}

impl MemoryRefAccess {
    pub(crate) fn new(memory: Arc<MemoryService>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl MemoryRefReader for MemoryRefAccess {
    async fn can_read(&self, session_id: &str, record_id: &str) -> bool {
        let Ok(id) = record_id.parse::<RecordId>() else {
            return false;
        };
        let Ok(Some(record)) = self.memory.get(id).await else {
            return false;
        };
        record.status == Status::Active
            && record.valid_to.is_none()
            && match record.scope.kind {
                ScopeKind::User => true,
                ScopeKind::Agent => record.scope.id == session_id,
                ScopeKind::Project => false,
            }
    }
}
