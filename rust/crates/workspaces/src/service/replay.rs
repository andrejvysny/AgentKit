//! A call whose operation already has a row never runs again (INV-3,
//! INV-7): a settled or uncertain row is read back, and a delivery that
//! finds the row of an earlier delivery that still runs the operation,
//! before or while it waits for the lease, waits for it (F-6) instead of
//! answering `sandbox_busy`.

use crate::error::SandboxError;
use crate::ids::WorkspaceId;
use crate::lease::{LeaseHolder, LeaseToken};
use crate::operation::{Operation, OperationPhase};
use crate::workspace::{Workspace, WorkspaceState};

use super::WorkspaceService;
use super::run::Plan;
use super::types::{FileRef, OpContext, OperationOutcome, Settlement};
use super::watch::Finished;

/// How waiting on an earlier delivery ended.
pub(super) enum Redelivery {
    /// The lease is free: this call adopts the open row under it.
    Leased(LeaseToken),
    /// The row ended (or this call stopped waiting): its outcome.
    Ended(OperationOutcome),
}

/// What a settled or uncertain row answers; nothing runs. `fenced` is the
/// workspace's state now.
pub(super) fn replay(row: &Operation, fenced: bool) -> OperationOutcome {
    let settlement = match row.phase {
        OperationPhase::Terminal(state) => Settlement::Settled(state),
        OperationPhase::Interrupted(state) => Settlement::Interrupted(state),
        _ => Settlement::Uncertain {
            reason: "an earlier delivery of this operation has no receipt; it is not run again"
                .into(),
            fenced,
        },
    };
    OperationOutcome {
        operation_id: row.id.clone(),
        workspace_id: row.workspace_id.clone(),
        generation: row.generation,
        image_digest: row.image_digest.clone(),
        settlement,
        output: Vec::new(),
        output_cut: false,
        log_truncated: row.truncated,
        log_path: row.log_path.clone(),
        written: None,
        network_denied: Vec::new(),
        replayed: true,
        started_at: row.started_at,
        ended_at: row.ended_at,
    }
}

/// The outcome of a watch of `row` by this call.
pub(super) fn outcome(
    row: &Operation,
    finished: Finished,
    written: Option<FileRef>,
) -> OperationOutcome {
    OperationOutcome {
        settlement: finished.settlement,
        output: finished.output,
        output_cut: finished.output_cut,
        log_truncated: finished.log_truncated,
        written,
        network_denied: finished.denied,
        replayed: false,
        ended_at: finished.ended_at,
        ..replay(row, false)
    }
}

impl WorkspaceService {
    /// `row` read back, with the fenced state of its workspace now.
    pub(super) async fn replay_row(&self, row: &Operation) -> OperationOutcome {
        let workspace = self.repo.workspace(&row.workspace_id).await;
        let fenced = workspace
            .ok()
            .flatten()
            .is_some_and(|ws| matches!(ws.state, WorkspaceState::Fenced { .. }));
        replay(row, fenced)
    }

    /// The lease for a mutating call. Waits up to the lease wait, then
    /// answers `Busy` naming the holder, unless this call's operation has a
    /// row (checked to be this call's, else `OperationConflict`): then an earlier delivery runs it (or ran it and is gone), and
    /// the wait goes on, past the lease wait, until the lease is free
    /// (adopt the row) or the row ends (read it back). Cancellation before
    /// the row exists is `Cancelled`; after, an uncertain outcome, since
    /// the operation may run on. A quit answers `sandbox_unavailable`.
    pub(super) async fn await_lease(
        &self,
        ctx: &OpContext,
        workspace: &Workspace,
        holder: &LeaseHolder,
        (plan, summary): (&Plan, Option<&str>),
    ) -> Result<Redelivery, SandboxError> {
        let started = self.now();
        let wait = i64::try_from(self.config.lease_wait_ms).unwrap_or(i64::MAX);
        loop {
            self.gate.check_open()?;
            // A row that is not this call's is a conflict, never a replay.
            let row = self.existing(ctx, workspace, plan, summary).await?;
            if let Some(row) = row.as_ref().filter(|row| !row.phase.is_open()) {
                return Ok(Redelivery::Ended(self.replay_row(row).await));
            }
            if ctx.cancellation.is_cancelled() {
                return match row {
                    None => Err(SandboxError::Cancelled),
                    Some(row) => Ok(Redelivery::Ended(self.stopped_waiting(&row).await)),
                };
            }
            if let Some(lease) = self.try_acquire(&workspace.id, holder).await? {
                return Ok(Redelivery::Leased(lease));
            }
            if row.is_none() && self.now().saturating_sub(started) >= wait {
                return Err(self.busy(&workspace.id).await?);
            }
            self.nap(self.config.poll_interval_ms, Some(&ctx.cancellation))
                .await;
        }
    }

    async fn stopped_waiting(&self, row: &Operation) -> OperationOutcome {
        let mut outcome = self.replay_row(row).await;
        outcome.settlement = Settlement::Uncertain {
            reason: "another delivery of this operation still runs it; this one stopped waiting"
                .into(),
            fenced: false,
        };
        outcome
    }

    /// `Busy` naming the current holder.
    async fn busy(&self, ws: &WorkspaceId) -> Result<SandboxError, SandboxError> {
        Ok(match self.repo.lease(ws, self.now()).await? {
            Some(info) => SandboxError::Busy {
                holder: info.holder,
            },
            None => SandboxError::Unavailable {
                reason: "the workspace lease changed hands while this call waited".into(),
            },
        })
    }
}
