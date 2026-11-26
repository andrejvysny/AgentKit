//! Machine lifecycle and owner actions: stop, recover, reset of
//! `/home/agent`, delete.
//!
//! Every stop asks the backend for a proof. A `Stopped` proof advances the
//! generation under the same lease, so the stopped generation never runs
//! again (INV-5); an unproven stop fences the workspace (INV-6) and records
//! the generation that may still run. Recover proves that generation and
//! moves past it and out of the fence in one transaction, so a crash can
//! never leave the generation advanced and the fence in place. One
//! relaxation: on a workspace that is not fenced and shows no evidence that
//! a machine of its current generation ever started, an unproven answer
//! means "nothing to stop" and changes nothing (a backend without a record
//! cannot prove absence, and there is no machine to fence against).

use tokio_util::sync::CancellationToken;

use crate::error::{RepositoryError, SandboxError};
use crate::ids::{Generation, WorkspaceId};
use crate::lease::LeaseToken;
use crate::machine::StopProof;
use crate::workspace::{Workspace, WorkspaceState};

use super::WorkspaceService;
use super::prepare::state_error;
use super::types::StopOutcome;
use super::watch::Watch;

/// What the stop proofs of a workspace showed.
enum Proof {
    /// Every generation that may have run is proven stopped; `next` is the
    /// first one past them.
    Stopped { next: Generation },
    /// Not fenced, no evidence of a start, no proof: nothing to stop.
    NotStarted,
    /// A generation that may run could not be proven stopped.
    Unproven {
        generation: Generation,
        reason: String,
    },
}

fn overflow() -> SandboxError {
    RepositoryError::InvalidInput {
        reason: "generation overflow".into(),
    }
    .into()
}

impl WorkspaceService {
    /// Stop the workspace's machine (an owner action). It waits for the
    /// lease like a writer, so it never stops a machine under a watched
    /// command. An open operation nobody watches is settled first: by its
    /// receipt (cancelled when still running), else uncertain. A fenced
    /// workspace answers `Fenced`: `recover` is its action.
    pub async fn stop(&self, ws: &WorkspaceId) -> Result<StopOutcome, SandboxError> {
        let holder = self.service_holder("stop");
        let lease = self.acquire_waiting(ws, &holder, None).await?;
        let result = self.stop_leased(&lease).await;
        self.release(&lease).await;
        result
    }

    async fn stop_leased(&self, lease: &LeaseToken) -> Result<StopOutcome, SandboxError> {
        let workspace = self.load(&lease.workspace_id).await?;
        if let WorkspaceState::Fenced { .. } | WorkspaceState::Deleted = workspace.state {
            return Err(state_error(&workspace.state, &workspace.id));
        }
        self.stop_current(lease).await
    }

    /// The owner's explicit recover (INV-6). On a fenced workspace: prove
    /// the generation the fence names stopped (and the current one too
    /// when the fence names a newer one and the current one ran here),
    /// then move past both and lift the fence in one transaction. Not
    /// proven: it stays fenced with the new reason. On any other workspace
    /// it is a stop.
    pub async fn recover(&self, ws: &WorkspaceId) -> Result<StopOutcome, SandboxError> {
        let holder = self.service_holder("recover");
        let lease = self.acquire_waiting(ws, &holder, None).await?;
        let result = self.recover_leased(&lease).await;
        self.release(&lease).await;
        result
    }

    async fn recover_leased(&self, lease: &LeaseToken) -> Result<StopOutcome, SandboxError> {
        let workspace = self.load(&lease.workspace_id).await?;
        match workspace.state {
            WorkspaceState::Deleted => Err(state_error(&workspace.state, &workspace.id)),
            WorkspaceState::Fenced { .. } => {
                self.settle_open(lease).await?;
                let workspace = self.load(&lease.workspace_id).await?;
                match self.prove(&workspace).await? {
                    Proof::Stopped { next } => {
                        self.renew(lease).await?;
                        let digest = &self.config.image_digest;
                        let (current, now) = (workspace.generation, self.now());
                        self.repo
                            .advance_and_unfence(lease, current, next, digest, now)
                            .await?;
                        Ok(StopOutcome::Stopped {
                            next_generation: next,
                        })
                    }
                    Proof::NotStarted => Ok(StopOutcome::NotStarted),
                    Proof::Unproven { generation, reason } => {
                        Ok(self.fence_outcome(lease, &reason, generation).await?)
                    }
                }
            }
            WorkspaceState::Ready | WorkspaceState::Busy => self.stop_current(lease).await,
        }
    }

    /// Empty `/home/agent` and keep `/workspace` ("Reset installed
    /// tools"). The machine is stopped first; a fenced workspace answers
    /// `Fenced`.
    pub async fn reset_home(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let holder = self.service_holder("reset home");
        let lease = self.acquire_waiting(ws, &holder, None).await?;
        let result = self.reset_home_leased(&lease).await;
        self.release(&lease).await;
        result
    }

    async fn reset_home_leased(&self, lease: &LeaseToken) -> Result<(), SandboxError> {
        if let StopOutcome::Fenced { reason } = self.stop_leased(lease).await? {
            return Err(SandboxError::Fenced { reason });
        }
        let call = self.backend.reset_home(&lease.workspace_id);
        self.bounded("reset_home", call).await
    }

    /// Delete the workspace: prove its machine stopped, destroy the
    /// storage, mark the record deleted (kept for attribution; the key can
    /// be used again). The generation is not advanced before the storage
    /// is gone. A failed destroy after a proven stop fences the workspace
    /// at the proven generation, so `recover` or another `delete` proves it
    /// again and goes on; with no machine ever started it changes nothing.
    pub async fn delete(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let holder = self.service_holder("delete");
        let lease = self.acquire_waiting(ws, &holder, None).await?;
        let result = self.delete_leased(&lease).await;
        self.release(&lease).await;
        result
    }

    async fn delete_leased(&self, lease: &LeaseToken) -> Result<(), SandboxError> {
        let workspace = self.load(&lease.workspace_id).await?;
        if workspace.state == WorkspaceState::Deleted {
            return Err(state_error(&workspace.state, &workspace.id));
        }
        self.settle_open(lease).await?;
        let workspace = self.load(&lease.workspace_id).await?;
        let proven = match self.prove(&workspace).await? {
            Proof::Unproven { generation, reason } => {
                return Err(self.fence_with(lease, &reason, generation).await);
            }
            Proof::Stopped { next } => Some(next),
            Proof::NotStarted => None,
        };
        self.renew(lease).await?;
        let call = self.backend.destroy_storage(&lease.workspace_id);
        if let Err(error) = self.bounded("destroy_storage", call).await {
            if let Some(next) = proven {
                let reason = format!("deleting the workspace failed: {error}");
                let at = Generation(next.0.saturating_sub(1));
                if let SandboxError::Repository(lost) = self.fence_with(lease, &reason, at).await {
                    return Err(lost.into());
                }
            }
            return Err(error);
        }
        self.renew(lease).await?;
        if workspace.state == WorkspaceState::Busy {
            // An uncertain operation of the proven generation keeps it busy.
            let digest = &self.config.image_digest;
            self.repo
                .advance_generation(lease, workspace.generation, digest, self.now())
                .await?;
        }
        self.repo.mark_deleted(lease, self.now()).await?;
        self.machines.forget(&lease.workspace_id);
        Ok(())
    }

    /// Under `lease`: settle an open operation (receipt or uncertain), then
    /// prove the current generation stopped and advance past it, or fence.
    /// A fenced workspace is left to `recover`.
    pub(super) async fn stop_current(
        &self,
        lease: &LeaseToken,
    ) -> Result<StopOutcome, SandboxError> {
        let workspace = self.load(&lease.workspace_id).await?;
        match &workspace.state {
            WorkspaceState::Deleted => return Err(state_error(&workspace.state, &workspace.id)),
            WorkspaceState::Fenced { reason, .. } => {
                self.machines.forget(&workspace.id);
                return Ok(StopOutcome::Fenced {
                    reason: reason.clone(),
                });
            }
            WorkspaceState::Ready | WorkspaceState::Busy => {}
        }
        self.settle_open(lease).await?;
        let workspace = self.load(&workspace.id).await?;
        if let WorkspaceState::Fenced { reason, .. } = &workspace.state {
            // Settling the open operation fenced it: recover is the action.
            return Ok(StopOutcome::Fenced {
                reason: reason.clone(),
            });
        }
        match self.prove(&workspace).await? {
            Proof::Stopped { .. } => {
                self.renew(lease).await?;
                let digest = &self.config.image_digest;
                let next = self
                    .repo
                    .advance_generation(lease, workspace.generation, digest, self.now())
                    .await?;
                Ok(StopOutcome::Stopped {
                    next_generation: next.generation,
                })
            }
            Proof::NotStarted => Ok(StopOutcome::NotStarted),
            Proof::Unproven { generation, reason } => {
                self.fence_outcome(lease, &reason, generation).await
            }
        }
    }

    async fn fence_outcome(
        &self,
        lease: &LeaseToken,
        reason: &str,
        generation: Generation,
    ) -> Result<StopOutcome, SandboxError> {
        match self.fence_with(lease, reason, generation).await {
            SandboxError::Fenced { reason } => Ok(StopOutcome::Fenced { reason }),
            error => Err(error),
        }
    }

    /// Prove stopped every generation of `workspace` that may run: the one
    /// a fence names, the current one when it is newer than that or when
    /// there is no fence, and the current one below a newer fenced one
    /// when it ran here. While fenced the service starts no machine, so a
    /// current generation above the fenced one never ran.
    async fn prove(&self, workspace: &Workspace) -> Result<Proof, SandboxError> {
        let current = workspace.generation;
        let (target, fenced) = match &workspace.state {
            WorkspaceState::Fenced { generation, .. } => (*generation, true),
            _ => (current, false),
        };
        let mut generations = vec![target];
        if target > current && self.has_evidence(workspace).await? {
            generations.push(current);
        }
        for generation in generations {
            if let StopProof::Unproven { reason } = self.stop_proof(&workspace.id, generation).await
            {
                if !fenced && !self.has_evidence(workspace).await? {
                    return Ok(Proof::NotStarted);
                }
                let reason = format!(
                    "the machine of generation {generation} could not be proven stopped: {reason}"
                );
                return Ok(Proof::Unproven { generation, reason });
            }
        }
        self.machines.forget(&workspace.id);
        let next = target.max(current).next().ok_or_else(overflow)?;
        Ok(Proof::Stopped { next })
    }

    /// An open operation whose watcher is gone (the lease was free):
    /// observe it on the machine this service runs; a receipt settles it,
    /// a running one is cancelled; with no such machine it is uncertain.
    pub(super) async fn settle_open(&self, lease: &LeaseToken) -> Result<(), SandboxError> {
        let Some(op) = self.open_operation(&lease.workspace_id).await? else {
            return Ok(());
        };
        let handle = self
            .machines
            .handle(&lease.workspace_id)
            .filter(|handle| handle.generation == op.generation);
        let w = Watch::new(self, &op, handle, lease);
        if self.watch(w, &CancellationToken::new(), true).await.lost {
            return Err(RepositoryError::LeaseLost {
                workspace_id: lease.workspace_id.clone(),
            }
            .into());
        }
        Ok(())
    }
}
