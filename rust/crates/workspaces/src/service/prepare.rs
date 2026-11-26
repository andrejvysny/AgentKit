//! Bringing a leased workspace to `Ready` with a live machine of its
//! current generation, and the generation rules on the way:
//!
//! - an open operation nobody watches (its holder is gone) is adopted:
//!   observed, cancelled, settled or made uncertain, never dispatched;
//! - an uncertain operation of the current generation means its machine
//!   must be proven stopped again, freshly, and only then does the
//!   generation advance (INV-5); an unproven stop fences (INV-6);
//! - a backend that refuses the current generation as stale (its machine
//!   was stopped) gets the same treatment, once; one that serves a newer
//!   generation than ours means another host runs it, and fences with that
//!   generation recorded, so recover proves it stopped and moves past it.

use crate::error::{RepositoryError, SandboxError};
use crate::ids::{Generation, OperationId, WorkspaceId};
use crate::lease::LeaseToken;
use crate::machine::{MachineHandle, MachineSpec, StopProof};
use crate::network::NetworkPolicy;
use crate::operation::Operation;
use crate::workspace::{Workspace, WorkspaceState};

use super::WorkspaceService;

/// Rounds of `mark_ready` before giving up: adopt, then bump, then ready.
const PREPARE_ROUNDS: usize = 4;

/// The error a refused state answers with.
pub(super) fn state_error(state: &WorkspaceState, id: &WorkspaceId) -> SandboxError {
    match state {
        WorkspaceState::Fenced { reason, .. } => SandboxError::Fenced {
            reason: reason.clone(),
        },
        WorkspaceState::Deleted => SandboxError::NotFound {
            what: format!("workspace {id}"),
        },
        other => SandboxError::Backend {
            reason: format!("workspace {id} is {other:?}"),
        },
    }
}

impl WorkspaceService {
    /// Under `lease`: settle what blocks the workspace, then ensure its
    /// machine. Answers the ready record and the machine.
    pub(super) async fn prepare(
        &self,
        lease: &LeaseToken,
    ) -> Result<(Workspace, MachineHandle), SandboxError> {
        for _ in 0..PREPARE_ROUNDS {
            match self.repo.mark_ready(lease, self.now()).await {
                Ok(workspace) => return self.leased_machine(lease, workspace).await,
                Err(RepositoryError::OperationOpen { operation_id, .. }) => {
                    self.adopt_orphan(lease, &operation_id).await?;
                }
                Err(RepositoryError::OperationUncertain { .. }) => {
                    let workspace = self.load(&lease.workspace_id).await?;
                    self.bump(lease, &workspace).await?;
                }
                Err(RepositoryError::WorkspaceStateConflict {
                    state,
                    workspace_id,
                }) => {
                    return Err(state_error(&state, &workspace_id));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(SandboxError::Backend {
            reason: format!("workspace {} did not become ready", lease.workspace_id),
        })
    }

    /// Adopt an open operation whose watcher is gone. Its outcome belongs
    /// to its own caller; here it only has to stop blocking.
    async fn adopt_orphan(&self, lease: &LeaseToken, id: &OperationId) -> Result<(), SandboxError> {
        let Some(op) = self.repo.operation(id).await? else {
            return Err(RepositoryError::OperationNotFound(id.clone()).into());
        };
        let finished = self.adopt(lease, &op, true).await?;
        if finished.lost {
            return Err(RepositoryError::LeaseLost {
                workspace_id: lease.workspace_id.clone(),
            }
            .into());
        }
        Ok(())
    }

    /// With a fresh `Stopped` proof for the current generation, advance
    /// past it (to the configured image); otherwise fence.
    pub(super) async fn bump(
        &self,
        lease: &LeaseToken,
        workspace: &Workspace,
    ) -> Result<(), SandboxError> {
        let generation = workspace.generation;
        match self.stop_proof(&workspace.id, generation).await {
            StopProof::Stopped => {
                self.machines.forget(&workspace.id);
                let digest = &self.config.image_digest;
                self.renew(lease).await?;
                self.repo
                    .advance_generation(lease, generation, digest, self.now())
                    .await?;
                Ok(())
            }
            StopProof::Unproven { reason } => {
                let reason = format!(
                    "the machine of generation {generation} could not be proven stopped: {reason}"
                );
                Err(self.fence_with(lease, &reason, generation).await)
            }
        }
    }

    /// Fence the leased workspace, recording `generation` as the newest one
    /// whose machine may still run, and forget its machine. The lease is
    /// renewed first: the stop proof before a fence may have waited long.
    /// Answers the error the caller returns.
    pub(super) async fn fence_with(
        &self,
        lease: &LeaseToken,
        reason: &str,
        generation: Generation,
    ) -> SandboxError {
        self.machines.forget(&lease.workspace_id);
        if let Err(error) = self.renew(lease).await {
            return error.into();
        }
        match self.repo.fence(lease, reason, generation, self.now()).await {
            Ok(_) => SandboxError::Fenced {
                reason: reason.to_owned(),
            },
            Err(error) => error.into(),
        }
    }

    /// The backend's stop proof; a failed or slow call proves nothing.
    pub(super) async fn stop_proof(&self, ws: &WorkspaceId, generation: Generation) -> StopProof {
        let call = self.backend.stop_machine(ws, generation);
        match self.bounded("stop_machine", call).await {
            Ok(proof) => proof,
            Err(error) => StopProof::Unproven {
                reason: error.to_string(),
            },
        }
    }

    /// The machine of a ready, leased workspace.
    async fn leased_machine(
        &self,
        lease: &LeaseToken,
        workspace: Workspace,
    ) -> Result<(Workspace, MachineHandle), SandboxError> {
        match self.ensure(&workspace).await {
            Err(SandboxError::StaleGeneration { current, .. })
                if current <= workspace.generation =>
            {
                self.bump(lease, &workspace).await?;
                let workspace = self.load(&workspace.id).await?;
                match self.ensure(&workspace).await {
                    Err(SandboxError::StaleGeneration { current, .. }) => {
                        Err(self.newer_elsewhere(lease, &workspace, current).await)
                    }
                    other => other.map(|handle| (workspace, handle)),
                }
            }
            Err(SandboxError::StaleGeneration { current, .. }) => {
                Err(self.newer_elsewhere(lease, &workspace, current).await)
            }
            other => other.map(|handle| (workspace, handle)),
        }
    }

    async fn newer_elsewhere(
        &self,
        lease: &LeaseToken,
        workspace: &Workspace,
        current: Generation,
    ) -> SandboxError {
        let reason = format!(
            "the backend serves generation {current} of this workspace, not {}: another host may run it",
            workspace.generation
        );
        self.fence_with(lease, &reason, current).await
    }

    /// `ensure_machine` for the workspace's current generation, after
    /// making room under the active-machine cap. A backend fault is
    /// `Unavailable`.
    ///
    /// The cap check, the start and its registration run under one
    /// in-instance lock, so two starts never both take the last slot.
    pub(super) async fn ensure(
        &self,
        workspace: &Workspace,
    ) -> Result<MachineHandle, SandboxError> {
        let _room = self.room.lock().await;
        self.make_room(&workspace.id).await?;
        let spec = self.spec(workspace).await?;
        let call = self.backend.ensure_machine(&spec);
        match self.bounded("ensure_machine", call).await {
            Ok(handle) => {
                self.machines.put(&handle, self.now());
                Ok(handle)
            }
            Err(SandboxError::Backend { reason }) => Err(SandboxError::Unavailable { reason }),
            Err(error) => Err(error),
        }
    }

    async fn spec(&self, workspace: &Workspace) -> Result<MachineSpec, SandboxError> {
        Ok(MachineSpec {
            workspace_id: workspace.id.clone(),
            generation: workspace.generation,
            image_digest: workspace.image_digest.clone(),
            limits: self.config.limits,
            network: self.policy(&workspace.id).await?,
        })
    }

    /// The egress policy of the workspace's active grants.
    pub(super) async fn policy(&self, ws: &WorkspaceId) -> Result<NetworkPolicy, SandboxError> {
        let grants = self.repo.list_active_grants(ws).await?;
        Ok(NetworkPolicy::from_grants(&grants))
    }

    /// Below the cap, or after stopping the least recently used idle
    /// machine. A busy one (its lease held) is never stopped.
    async fn make_room(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let max = self.config.max_active_machines.max(1);
        if self.machines.handle(ws).is_some() || self.machines.count() < max {
            return Ok(());
        }
        for victim in self.machines.least_recently_used(Some(ws)) {
            self.evict(&victim, "machine cap").await?;
            if self.machines.count() < max {
                return Ok(());
            }
        }
        Err(SandboxError::Unavailable {
            reason: format!("all {max} active sandbox machines are busy"),
        })
    }

    /// Stop `ws`'s machine when nobody holds its lease. Answers whether it
    /// was stopped.
    pub(super) async fn evict(
        &self,
        ws: &WorkspaceId,
        purpose: &str,
    ) -> Result<bool, SandboxError> {
        let outcome = self.stop_unleased(ws, purpose).await?;
        Ok(matches!(outcome, Some(super::StopOutcome::Stopped { .. })))
    }

    /// `stop_current` under a lease taken only if it is free; `None` when
    /// it is held. A deleted workspace is forgotten.
    pub(super) async fn stop_unleased(
        &self,
        ws: &WorkspaceId,
        purpose: &str,
    ) -> Result<Option<super::StopOutcome>, SandboxError> {
        let holder = self.service_holder(purpose);
        let Some(lease) = self.try_acquire(ws, &holder).await? else {
            return Ok(None);
        };
        let outcome = self.stop_current(&lease).await;
        self.release(&lease).await;
        if let Err(SandboxError::NotFound { .. }) = outcome {
            // Deleted elsewhere: no machine of it runs here any more.
            self.machines.forget(ws);
        }
        outcome.map(Some)
    }

    /// The open operation of `ws`, if any (at most one, INV-4).
    pub(super) async fn open_operation(
        &self,
        ws: &WorkspaceId,
    ) -> Result<Option<Operation>, SandboxError> {
        let open = self.repo.list_open_operations().await?;
        Ok(open.into_iter().find(|op| &op.workspace_id == ws))
    }

    /// Evidence that a machine of the current generation was started: this
    /// instance runs it, or an operation row names the generation (a row
    /// is written only after its machine was ensured).
    pub(super) async fn has_evidence(&self, workspace: &Workspace) -> Result<bool, SandboxError> {
        if self.machines.runs(&workspace.id, workspace.generation) {
            return Ok(true);
        }
        let latest = self.repo.list_operations(&workspace.id, 1).await?;
        Ok(latest
            .first()
            .is_some_and(|op| op.generation == workspace.generation))
    }
}
