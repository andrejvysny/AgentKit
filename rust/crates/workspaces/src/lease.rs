//! The exclusive per-workspace lease: one writer per workspace across runs
//! and agents (D-23, INV-4). Every state-changing write of a workspace or
//! operation presents a [`LeaseToken`]; the repository checks it in the
//! same transaction, like the engine's fenced task writes.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, WorkspaceId};

/// Who holds or asks for a lease. Shown to a waiting caller as
/// `sandbox_busy`.
///
/// `host_id` and `boot_id` name the service instance that took the lease.
/// Startup recovery breaks only expired leases and leases of earlier boots
/// of its own host: one live service instance per host id is assumed, and
/// a live lease of another host sharing the store is left alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseHolder {
    /// The agent.
    pub agent_id: AgentId,
    /// The run.
    pub run_id: String,
    /// Display label, for example the agent's name.
    pub label: String,
    /// The host the holding service runs on (stable across restarts).
    pub host_id: String,
    /// The boot of that service (new at every start).
    pub boot_id: String,
}

/// Proof of holding a workspace lease. A token is current when its `token`
/// and `fencing` equal the stored lease and the stored lease's
/// `expires_at > now` (this struct's copy is informational); any other
/// token is stale and every write presenting it fails with
/// [`crate::RepositoryError::LeaseLost`] without changing anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseToken {
    /// The leased workspace.
    pub workspace_id: WorkspaceId,
    /// Caller-minted token text (through the runtime identity port).
    pub token: String,
    /// Strictly increasing per workspace across acquire, release and break,
    /// so a token from an earlier holder can never become current again.
    pub fencing: u64,
    /// Epoch milliseconds.
    pub expires_at: i64,
}

/// The stored lease of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseInfo {
    /// The leased workspace.
    pub workspace_id: WorkspaceId,
    /// Holder.
    pub holder: LeaseHolder,
    /// Fencing number of the lease.
    pub fencing: u64,
    /// Epoch milliseconds; at or before `now` the lease is expired.
    pub expires_at: i64,
}

/// Result of an acquire attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseAcquire {
    /// The caller holds the lease.
    Acquired(LeaseToken),
    /// Another unexpired lease holds the workspace.
    Held {
        /// The current holder.
        holder: LeaseHolder,
        /// When its lease expires unless renewed.
        expires_at: i64,
    },
}
