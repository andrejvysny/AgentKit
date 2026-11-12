//! Durable-fact ports for the native lane (AGENTKIT-112/115, phase N1).
//!
//! The records below are the wire/durable shapes of the two ledgers the
//! generic executor reads and writes. The SQLite store implements
//! [`NativeRunRepository`] by forwarding to its inherent methods; the
//! executor never names a concrete store.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One row of the vendor-session lineage ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSessionBinding {
    pub attempt_id: String,
    pub task_id: String,
    pub backend: String,
    pub vendor_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_vendor_session_id: Option<String>,
    /// `assigned` (bound before spawn from the frozen plan), `observed`
    /// (confirmed from the vendor init frame) or `retired`.
    pub state: String,
    pub assigned_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<i64>,
}

/// Pre-spawn binding input: the identity the plan froze, recorded before
/// any process exists (AGENTKIT-113 crash-mid-attempt lineage).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindNativeSession {
    pub attempt_id: String,
    pub task_id: String,
    pub backend: String,
    pub vendor_session_id: String,
    pub parent_vendor_session_id: Option<String>,
    pub assigned_at: i64,
}

/// One row of the scoped continuation ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeContinuationBinding {
    pub binding_id: String,
    /// Consumer-defined conversation identity (opaque to the host).
    pub conversation: String,
    /// Consumer-defined selected-parent reference (e.g. parent message id).
    pub parent: String,
    /// Acknowledged history position at commit (monotonic per conversation).
    pub history_position: i64,
    pub backend: String,
    pub executable: String,
    pub config_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_revision: Option<String>,
    pub vendor_session_id: String,
    pub owning_attempt_id: String,
    pub owning_task_id: String,
    /// `active` | `owned` | `uncertain` | `superseded` | `retired`
    /// (review round-4 F3). `active` = available for one atomic claim at
    /// the verified position; `owned` = claimed by exactly one attempt
    /// (`claimed_attempt_id`) whose input may already have advanced the
    /// vendor session — never selectable by another attempt; `uncertain` =
    /// the claiming attempt was interrupted AFTER its input was accepted,
    /// so exact resume from this pre-claim position is blocked until
    /// reconciled (or the conversation moves on through a fresh session).
    pub state: String,
    pub committed_at: i64,
    /// The attempt that holds (or held) the exclusive claim, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<i64>,
    /// Monotonic compare-and-set counter: every claim/release/uncertain/
    /// reconcile transition bumps it, so a claim keyed on a stale
    /// generation loses the race instead of overwriting (F3-C: exactly
    /// one winner).
    pub generation: i64,
}

/// Commit input for one continuation binding (frozen facts from the
/// completed attempt's target + payload scope).
///
/// `parent`/`history_position` are the COMMIT (completed-output) position:
/// where the NEXT turn attaches when it resumes this session — never the
/// input position the turn was admitted from. A vendor session is a mutable
/// cursor through conversation history; committing it at its input position
/// would let regeneration select the session that already contains the
/// discarded answer (review §2.3 / AGENTKIT-113).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitContinuation {
    pub conversation: String,
    pub parent: String,
    pub history_position: i64,
    pub backend: String,
    pub executable: String,
    pub config_identity: String,
    pub system_prompt_revision: Option<String>,
    pub vendor_session_id: String,
    pub owning_attempt_id: String,
    pub owning_task_id: String,
    pub committed_at: i64,
}

/// Durable facts the native executor reads and writes: the vendor-session
/// lineage ledger and the scoped continuation ledger. Implemented by the
/// SQLite store. The executor never names a concrete store.
#[async_trait]
pub trait NativeRunRepository: Clone + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn bind_native_session(
        &self,
        input: BindNativeSession,
    ) -> Result<NativeSessionBinding, Self::Error>;
    async fn observe_native_session(
        &self,
        attempt_id: &str,
        vendor_session_id: &str,
        now: i64,
    ) -> Result<NativeSessionBinding, Self::Error>;
    async fn current_native_binding(
        &self,
        task_id: &str,
    ) -> Result<Option<NativeSessionBinding>, Self::Error>;
    async fn invalidate_interactions(
        &self,
        attempt_id: &str,
        now: i64,
    ) -> Result<usize, Self::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn active_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn claim_selected_continuation(
        &self,
        selected: &NativeContinuationBinding,
        claimant_task_id: &str,
        claimant_attempt_id: &str,
        now: i64,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn commit_continuation(
        &self,
        input: CommitContinuation,
    ) -> Result<NativeContinuationBinding, Self::Error>;
    #[allow(clippy::too_many_arguments)]
    async fn resumable_continuation(
        &self,
        conversation: &str,
        parent: &str,
        history_position: i64,
        backend: &str,
        executable: &str,
        config_identity: &str,
        system_prompt_revision: Option<&str>,
        attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn release_continuation_claim(
        &self,
        binding_id: &str,
        claimant_attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error>;
    async fn mark_continuation_uncertain(
        &self,
        binding_id: &str,
        claimant_attempt_id: &str,
    ) -> Result<Option<NativeContinuationBinding>, Self::Error>;
}
