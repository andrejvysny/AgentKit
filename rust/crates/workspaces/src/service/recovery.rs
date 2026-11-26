//! Startup recovery (spec "Host crash") and idle stop.
//!
//! One live service instance per host id is assumed. At start, recovery
//! breaks the leases whose holders are gone for sure: expired ones, and
//! those of earlier boots of this host. A live lease of another host
//! sharing the store is left alone, and so are its workspaces.

use crate::error::SandboxError;
use crate::ids::WorkspaceId;
use crate::lease::LeaseToken;
use crate::operation::Operation;
use crate::workspace::WorkspaceState;

use super::WorkspaceService;
use super::types::{RecoveryReport, Settlement, StopOutcome};
use super::watch::{Finished, Watch};

impl WorkspaceService {
    /// Startup recovery. Run before the host serves calls.
    ///
    /// 1. Leases of gone holders are broken (see the module docs).
    /// 2. Each open operation whose lease is free has its machine stopped
    ///    and becomes uncertain (never re-run), or is fenced when the stop
    ///    is unproven. A backend whose descriptor says it answers status
    ///    after host loss is observed first, and a receipt settles the
    ///    operation (F-4). A lease without an open operation means nothing
    ///    was dispatched (F-1).
    /// 3. Every other workspace that is neither deleted nor fenced, whose
    ///    lease is free, has its current generation stopped: a machine left
    ///    over from before the restart is proven stopped and the generation
    ///    advances. An unproven stop fences only where an operation row
    ///    shows that a machine of that generation ran.
    pub async fn recover_all(&self) -> Result<RecoveryReport, SandboxError> {
        let mut report = RecoveryReport::default();
        let now = self.now();
        for info in self.repo.list_leases().await? {
            let gone = info.expires_at <= now || self.earlier_boot(&info.holder);
            if gone
                && self
                    .repo
                    .break_lease(&info.workspace_id, info.fencing)
                    .await?
            {
                report.leases_broken += 1;
            }
        }
        let mut handled: Vec<WorkspaceId> = Vec::new();
        for op in self.repo.list_open_operations().await? {
            handled.push(op.workspace_id.clone());
            let holder = self.service_holder("recovery");
            let Some(lease) = self.try_acquire(&op.workspace_id, &holder).await? else {
                continue;
            };
            let finished = self.recover_open(&lease, &op).await;
            self.release(&lease).await;
            match finished?.settlement {
                Settlement::Uncertain { fenced, .. } => {
                    report.uncertain.push(op.id.clone());
                    if fenced {
                        report.fenced.push(op.workspace_id.clone());
                    }
                }
                Settlement::Settled(_) | Settlement::Interrupted(_) => report.settled.push(op.id),
            }
        }
        self.sweep(&handled, &mut report).await?;
        Ok(report)
    }

    /// Step 3 of `recover_all`.
    async fn sweep(
        &self,
        skip: &[WorkspaceId],
        report: &mut RecoveryReport,
    ) -> Result<(), SandboxError> {
        for workspace in self.repo.list_workspaces().await? {
            let idle = matches!(
                workspace.state,
                WorkspaceState::Ready | WorkspaceState::Busy
            );
            if !idle || skip.contains(&workspace.id) {
                continue;
            }
            match self.stop_unleased(&workspace.id, "recovery").await? {
                Some(StopOutcome::Stopped { .. }) => report.stopped.push(workspace.id),
                Some(StopOutcome::Fenced { .. }) => report.fenced.push(workspace.id),
                Some(StopOutcome::NotStarted) | None => {}
            }
        }
        Ok(())
    }

    async fn recover_open(
        &self,
        lease: &LeaseToken,
        op: &Operation,
    ) -> Result<Finished, SandboxError> {
        if self.backend.describe().supports_status_after_host_loss {
            return self.adopt(lease, op, true).await;
        }
        let reason = "the host restarted before the operation reported back";
        Ok(self
            .give_up(Watch::new(self, op, None, lease), reason)
            .await)
    }

    /// Stop every machine this service runs that was last used at or
    /// before `now - idle_stop_ms`, least recently used first. A busy one
    /// (its lease held) is skipped. Answers the workspaces stopped.
    pub async fn evict_idle(&self, now: i64) -> Result<Vec<WorkspaceId>, SandboxError> {
        let cutoff = now.saturating_sub(self.config.idle_stop_ms);
        let mut stopped = Vec::new();
        for ws in self.machines.idle_since(cutoff) {
            if self.evict(&ws, "idle stop").await? {
                stopped.push(ws);
            }
        }
        Ok(stopped)
    }
}
