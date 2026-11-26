//! Network grants and requests (owner side). A grant belongs to the
//! workspace, so a host approved for one member is approved for all. A
//! change reaches a machine this service runs at once (NET-6); every
//! operation re-applies the policy before it starts in any case. A denial
//! after the owner rejected the target counts on the rejected request and
//! the owner is not asked again (the repository's `record_denial` rule).
//!
//! Policy pushes to one workspace's machine are serialized, and each one
//! reads the grants inside that lock: a push that read the grants before a
//! revoke can never land after the revoke's own push and undo it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::error::{RepositoryError, SandboxError};
use crate::ids::{GrantId, RequestId, WorkspaceId};
use crate::limits::PENDING_REQUESTS_MAX;
use crate::machine::MachineHandle;
use crate::network::{
    GrantSource, HostPort, NetworkGrant, NetworkRequest, RequestDecided, RequestDecision,
};

use super::WorkspaceService;
use super::types::NetworkDecision;

/// One push lock per workspace this instance pushed a policy to.
#[derive(Default)]
pub(super) struct Pushes {
    locks: Mutex<HashMap<WorkspaceId, Arc<tokio::sync::Mutex<()>>>>,
}

impl Pushes {
    fn lock_of(&self, ws: &WorkspaceId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.entry(ws.clone()).or_default().clone()
    }
}

impl WorkspaceService {
    /// Allow `ws` to connect to `target`. An active grant for the target is
    /// returned instead of a duplicate.
    pub async fn grant(
        &self,
        ws: &WorkspaceId,
        target: &HostPort,
        source: GrantSource,
    ) -> Result<NetworkGrant, SandboxError> {
        let grant = self.new_grant(ws, target, source);
        let stored = self.repo.insert_grant(&grant).await?;
        self.apply_live(ws).await?;
        Ok(stored)
    }

    /// Revoke a grant. A machine this service runs loses the target at
    /// once; when that live change fails the error says so (the grant is
    /// revoked either way and the next operation applies it).
    pub async fn revoke(&self, grant: &GrantId) -> Result<NetworkGrant, SandboxError> {
        let revoked = self.repo.revoke_grant(grant, self.now()).await?;
        self.apply_live(&revoked.workspace_id).await?;
        Ok(revoked)
    }

    /// Decide a pending request of `ws`. `Approve` creates the grant with
    /// the decision and applies it live. A request that is not pending in
    /// `ws` is `RequestNotFound` and nothing changes.
    pub async fn decide_request(
        &self,
        ws: &WorkspaceId,
        request: &RequestId,
        decision: NetworkDecision,
    ) -> Result<RequestDecided, SandboxError> {
        let pending = self.repo.list_pending_requests(ws).await?;
        let Some(found) = pending.iter().find(|pending| &pending.id == request) else {
            return Err(RepositoryError::RequestNotFound(request.clone()).into());
        };
        let decision = match decision {
            NetworkDecision::Reject => RequestDecision::Reject,
            NetworkDecision::Approve => RequestDecision::Approve {
                grant: self.new_grant(ws, &found.target, GrantSource::Owner),
            },
        };
        let decided = self.repo.decide_request(request, &decision).await?;
        if decided.grant.is_some() {
            self.apply_live(ws).await?;
        }
        Ok(decided)
    }

    /// Requests of `ws` waiting for the owner, oldest first, at most
    /// `PENDING_REQUESTS_MAX`. The tool layer parks a run's next sandbox
    /// call on them.
    pub async fn pending_requests(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Vec<NetworkRequest>, SandboxError> {
        let mut pending = self.repo.list_pending_requests(ws).await?;
        pending.truncate(PENDING_REQUESTS_MAX);
        Ok(pending)
    }

    fn new_grant(&self, ws: &WorkspaceId, target: &HostPort, source: GrantSource) -> NetworkGrant {
        NetworkGrant {
            id: GrantId::mint(&*self.runtime),
            workspace_id: ws.clone(),
            target: target.clone(),
            source,
            granted_at: self.now(),
            revoked_at: None,
        }
    }

    /// Push the workspace's policy to the machine this service runs for it.
    async fn apply_live(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let Some(handle) = self.machines.handle(ws) else {
            return Ok(());
        };
        self.push_policy(ws, &handle).await
    }

    /// Under the workspace's push lock: read the active grants, then push
    /// them to `handle`'s machine.
    pub(super) async fn push_policy(
        &self,
        ws: &WorkspaceId,
        handle: &MachineHandle,
    ) -> Result<(), SandboxError> {
        let lock = self.pushes.lock_of(ws);
        let _serialized = lock.lock().await;
        let policy = self.policy(ws).await?;
        let call = self.backend.apply_network_policy(handle, &policy);
        self.bounded("apply_network_policy", call).await
    }
}
