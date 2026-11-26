//! The workspace record and its members. A workspace is the durable thing
//! (volume, policy, member list); a machine is disposable and never
//! persisted as truth, its generation is.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, Generation, WorkspaceId};
use crate::network::NetworkGrant;

/// What a workspace serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// One agent's persistent home.
    AgentHome,
    /// All chats of one project (or of one OneMind workspace outside any
    /// project), one folder per chat (D-35).
    ProjectChats,
    /// One chat that opted out of the shared project workspace.
    ChatIsolated,
    /// An owner-created workspace with several member agents.
    Shared,
    /// Reserved for a mounted host folder (D-17). Not built: no service path
    /// creates it and backends refuse it.
    HostFolder,
}

/// Lifecycle state of a workspace record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkspaceState {
    /// No mutating operation is open; a writer may be admitted.
    Ready,
    /// A mutating operation is open (INV-4).
    Busy,
    /// An outcome is unknown and the stop of the last machine is not proven.
    /// No operation is admitted until an explicit recover proves the stop
    /// (INV-6). Same concept as the coding coordinator's quarantine.
    Fenced {
        /// Why the workspace was fenced, for the owner.
        reason: String,
        /// The newest generation whose machine may still run: recover must
        /// prove it stopped and move the workspace past it. Usually the
        /// generation current when the fence was entered; higher when a
        /// backend served a newer generation than the record.
        generation: Generation,
    },
    /// Storage destroyed; the record stays for attribution.
    Deleted,
}

impl WorkspaceState {
    /// True when a new mutating operation may be admitted.
    pub fn admits_operations(&self) -> bool {
        matches!(self, WorkspaceState::Ready)
    }
}

/// The unique key a turn policy pins, for example `agent:<id>` or
/// `chats:project:<id>`. The host computes it; it never comes from model
/// arguments (D-24).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceKey(String);

impl WorkspaceKey {
    /// Wrap a host-computed key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A workspace record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    /// Identity, minted at first use of the key.
    pub id: WorkspaceId,
    /// What it serves.
    pub kind: WorkspaceKind,
    /// Unique among records that are not `Deleted`.
    pub key: WorkspaceKey,
    /// The owner scope the host resolves authority against (opaque here).
    pub owner_scope: String,
    /// Lifecycle state.
    pub state: WorkspaceState,
    /// Current machine generation.
    pub generation: Generation,
    /// Image digest the current generation runs.
    pub image_digest: String,
    /// Set by the first copy-in of user files; arms the disclosure gate.
    pub holds_user_files: bool,
    /// Epoch milliseconds.
    pub created_at: i64,
    /// Epoch milliseconds of the last operation or read.
    pub last_used_at: i64,
}

/// Input of [`crate::WorkspaceRepository::create_or_get_workspace`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkspace {
    /// Id to use if the key has no record yet.
    pub id: WorkspaceId,
    /// Kind of the new record.
    pub kind: WorkspaceKind,
    /// Unique key.
    pub key: WorkspaceKey,
    /// Owner scope.
    pub owner_scope: String,
    /// Image digest of the first generation.
    pub image_digest: String,
    /// Grants created with the record, in the same transaction; normally
    /// [`crate::default_grants`]. Their `workspace_id` must equal `id`.
    pub initial_grants: Vec<NetworkGrant>,
    /// Epoch milliseconds; becomes `created_at` and `last_used_at`.
    pub now: i64,
}

/// Result of an atomic create-or-get by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceUpsert {
    /// The one record for the key.
    pub workspace: Workspace,
    /// True when this call created it (and its initial grants).
    pub created: bool,
}

/// What a member may do. Only `Member` is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// Full use of the workspace.
    Member,
    /// Reserved: read-only member. Not built.
    Reader,
}

/// An agent allowed to use a workspace. Membership is checked at admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    /// The workspace.
    pub workspace_id: WorkspaceId,
    /// The agent.
    pub agent_id: AgentId,
    /// Its role.
    pub role: MemberRole,
    /// Epoch milliseconds.
    pub added_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_and_kind_use_snake_case_wire_names() {
        let fenced = WorkspaceState::Fenced {
            reason: "stop not proven".into(),
            generation: Generation(3),
        };
        assert_eq!(
            serde_json::to_value(&fenced).unwrap(),
            json!({"state": "fenced", "reason": "stop not proven", "generation": 3})
        );
        assert_eq!(
            serde_json::to_value(WorkspaceKind::ProjectChats).unwrap(),
            json!("project_chats")
        );
        assert!(WorkspaceState::Ready.admits_operations());
        assert!(!fenced.admits_operations());
        assert!(!WorkspaceState::Busy.admits_operations());
    }
}
