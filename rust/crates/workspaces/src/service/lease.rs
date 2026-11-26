//! The workspace lease as the service uses it: wait for it up to the
//! limit, try it once (eviction, recovery), renew it while an operation
//! runs, release it on every exit. Release needs only the token and
//! fencing number, so the token from the acquire releases a renewed lease.

use tokio_util::sync::CancellationToken;

use crate::error::{RepositoryError, SandboxError};
use crate::ids::{AgentId, WorkspaceId};
use crate::lease::{LeaseAcquire, LeaseHolder, LeaseToken};

use super::WorkspaceService;
use super::types::OpContext;

/// True when `error` says the caller's lease is no longer current.
pub(super) fn is_lease_lost(error: &RepositoryError) -> bool {
    matches!(error, RepositoryError::LeaseLost { .. })
}

/// The error of a fenced step whose lease is gone.
pub(super) fn lost(workspace_id: &WorkspaceId) -> SandboxError {
    RepositoryError::LeaseLost {
        workspace_id: workspace_id.clone(),
    }
    .into()
}

/// What a renewal found.
pub(super) enum Renewal {
    /// The lease is current again, until the new expiry.
    Kept(LeaseToken),
    /// Another holder took over; write nothing more.
    Lost,
    /// The store failed; the lease may or may not be current.
    Failed(String),
}

impl WorkspaceService {
    /// The holder of an agent's mutating call.
    pub(super) fn agent_holder(&self, ctx: &OpContext) -> LeaseHolder {
        self.holder(
            ctx.agent_id.clone(),
            ctx.run_id.clone(),
            ctx.agent_id.to_string(),
        )
    }

    /// The holder of an owner or maintenance action (stop, recover, evict).
    pub(super) fn service_holder(&self, purpose: &str) -> LeaseHolder {
        let agent = AgentId::new("agentkit:sandbox");
        self.holder(agent, purpose.to_owned(), format!("sandbox {purpose}"))
    }

    fn holder(&self, agent_id: AgentId, run_id: String, label: String) -> LeaseHolder {
        LeaseHolder {
            agent_id,
            run_id,
            label,
            host_id: self.config.host_id.clone(),
            boot_id: self.boot_id.clone(),
        }
    }

    /// True when `holder` is gone for sure: an earlier boot of this host.
    pub(super) fn earlier_boot(&self, holder: &LeaseHolder) -> bool {
        holder.host_id == self.config.host_id && holder.boot_id != self.boot_id
    }

    /// Take the lease of `ws`, waiting up to the lease wait. Answers
    /// `Busy` naming the holder after the wait, `Cancelled` when `cancel`
    /// fires first, `Unavailable` once the service quits.
    pub(super) async fn acquire_waiting(
        &self,
        ws: &WorkspaceId,
        holder: &LeaseHolder,
        cancel: Option<&CancellationToken>,
    ) -> Result<LeaseToken, SandboxError> {
        let token = self.runtime.id();
        let started = self.now();
        let wait = i64::try_from(self.config.lease_wait_ms).unwrap_or(i64::MAX);
        loop {
            self.gate.check_open()?;
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Err(SandboxError::Cancelled);
            }
            let now = self.now();
            let ttl = self.config.lease_ttl_ms;
            match self
                .repo
                .acquire_lease(ws, holder, &token, now, ttl)
                .await?
            {
                LeaseAcquire::Acquired(lease) => return Ok(lease),
                LeaseAcquire::Held { holder, .. } => {
                    if now.saturating_sub(started) >= wait {
                        return Err(SandboxError::Busy { holder });
                    }
                }
            }
            self.nap(self.config.poll_interval_ms, cancel).await;
        }
    }

    /// Take the lease of `ws` if nobody holds it; `None` when held.
    pub(super) async fn try_acquire(
        &self,
        ws: &WorkspaceId,
        holder: &LeaseHolder,
    ) -> Result<Option<LeaseToken>, SandboxError> {
        let token = self.runtime.id();
        let now = self.now();
        let ttl = self.config.lease_ttl_ms;
        match self
            .repo
            .acquire_lease(ws, holder, &token, now, ttl)
            .await?
        {
            LeaseAcquire::Acquired(lease) => Ok(Some(lease)),
            LeaseAcquire::Held { .. } => Ok(None),
        }
    }

    /// Extend `lease`; `LeaseLost` when another holder took over.
    pub(super) async fn renew(&self, lease: &LeaseToken) -> Result<LeaseToken, RepositoryError> {
        let now = self.now();
        self.repo
            .renew_lease(lease, now, self.config.lease_ttl_ms)
            .await
    }

    /// Renew `lease` and say what that found.
    pub(super) async fn refresh(&self, lease: &LeaseToken) -> Renewal {
        match self.renew(lease).await {
            Ok(lease) => Renewal::Kept(lease),
            Err(error) if is_lease_lost(&error) => Renewal::Lost,
            Err(error) => Renewal::Failed(error.to_string()),
        }
    }

    /// True when `lease` has no more than the longest wait between two
    /// lease checks left, so it must be renewed before the next one.
    pub(super) fn lease_due(&self, lease: &LeaseToken) -> bool {
        let left = lease.expires_at.saturating_sub(self.now());
        u64::try_from(left).unwrap_or(0) <= self.config.renewal_gap_ms()
    }

    /// Drop `lease`. A lease already lost or broken is left to its new
    /// holder; nothing else can fail here that the caller could act on.
    pub(super) async fn release(&self, lease: &LeaseToken) {
        let _ = self.repo.release_lease(lease).await;
    }
}
