//! After the operation row exists: the one dispatch, the observe loop, and
//! the two ways an operation ends here.
//!
//! - **Settle**: a receipt was read; it is written once (INV-7) and the
//!   workspace is ready again. After a quit cancelled it, the receipt is
//!   recorded as `Interrupted`.
//! - **Give up**: no receipt can be read (dispatch error or timeout, an
//!   observe error, the guest has no record, a cancel without a receipt
//!   inside the grace). The row becomes `Uncertain`, then the denials are
//!   drained and the machine of its generation is stopped. A proven stop
//!   leaves the workspace `Busy` with the uncertain row, so the next use
//!   advances the generation; an unproven stop fences it.
//!
//! Nothing here answers an error: once a row may have been dispatched the
//! caller always gets an outcome, uncertain when anything failed (SBX-07).
//! On this path the lease is checked before every step that may wait on the
//! backend and renewed when less than the longest such step is left, and
//! renewed again right before each fenced write that ends the operation,
//! so a slow backend never outlives it (`ServiceConfig::validate` bounds
//! the TTL by that step). When a fenced write answers `LeaseLost`,
//! another worker owns the operation: this one stops touching it (no write,
//! no stop, no fence) and reports the outcome as uncertain.

use tokio_util::sync::CancellationToken;

use crate::error::SandboxError;
use crate::lease::LeaseToken;
use crate::limits::OBSERVE_CHUNK_MAX_BYTES;
use crate::machine::{DispatchAck, MachineHandle, Observation, StopProof};
use crate::network::HostPort;
use crate::operation::{
    Operation, OperationPhase, OperationRequest, OperationState, OperationUpdate,
};

use super::WorkspaceService;
use super::lease::{Renewal, is_lease_lost};
use super::output::Tail;
use super::types::Settlement;

/// What the agent is told when its worker lost the lease mid-operation.
const LEASE_LOST: &str = "the workspace lease was lost while the operation ran; another worker owns it now and this one wrote nothing more";

/// How a watched operation ended.
pub(super) struct Finished {
    pub(super) settlement: Settlement,
    pub(super) output: Vec<u8>,
    pub(super) output_cut: bool,
    pub(super) log_truncated: bool,
    pub(super) denied: Vec<HostPort>,
    pub(super) ended_at: Option<i64>,
    /// The lease was lost: nothing more was written, nothing stopped.
    pub(super) lost: bool,
}

/// How the stop after an uncertain outcome ended.
enum AfterStop {
    Stopped,
    Fenced(String),
    NotFenced(String),
}

/// One operation being watched under a lease.
pub(super) struct Watch<'a> {
    op: &'a Operation,
    handle: Option<MachineHandle>,
    lease: LeaseToken,
    tail: Tail,
    seq: u64,
    log_truncated: bool,
    cancel_at: Option<i64>,
    /// The cancel came from the service's quit: a receipt is `Interrupted`.
    quit: bool,
}

impl<'a> Watch<'a> {
    pub(super) fn new(
        service: &WorkspaceService,
        op: &'a Operation,
        handle: Option<MachineHandle>,
        lease: &LeaseToken,
    ) -> Self {
        Self {
            op,
            handle,
            lease: lease.clone(),
            tail: Tail::new(service.config.model_output),
            seq: 0,
            log_truncated: false,
            cancel_at: None,
            quit: false,
        }
    }

    fn absorb(&mut self, seen: &Observation) {
        self.seq = self.seq.max(seen.next_seq);
        let max = usize::try_from(OBSERVE_CHUNK_MAX_BYTES).unwrap_or(usize::MAX);
        let skip = seen.output.len().saturating_sub(max);
        self.tail.push(&seen.output[skip..]);
        self.log_truncated |= seen.truncated;
    }

    fn finish(
        self,
        settlement: Settlement,
        denied: Vec<HostPort>,
        ended_at: Option<i64>,
    ) -> Finished {
        let (output, output_cut) = self.tail.into_parts();
        Finished {
            settlement,
            output,
            output_cut,
            log_truncated: self.log_truncated,
            denied,
            ended_at,
            lost: false,
        }
    }

    /// The end of a watch whose lease was lost.
    pub(super) fn lost(self) -> Finished {
        let settlement = Settlement::Uncertain {
            reason: LEASE_LOST.to_owned(),
            fenced: false,
        };
        Finished {
            lost: true,
            ..self.finish(settlement, Vec::new(), None)
        }
    }

    /// An uncertain end that could not be recorded: the row stays open and
    /// the next lease holder adopts it.
    fn unrecorded(self, reason: &str, error: &str, now: i64) -> Finished {
        let settlement = Settlement::Uncertain {
            reason: format!("{reason}; the outcome could not be recorded ({error})"),
            fenced: false,
        };
        self.finish(settlement, Vec::new(), Some(now))
    }

    /// Renew the lease; false when it is lost (a store fault lets the
    /// fenced write itself decide).
    async fn refresh(&mut self, service: &WorkspaceService) -> bool {
        match service.refresh(&self.lease).await {
            Renewal::Kept(lease) => self.lease = lease,
            Renewal::Lost => return false,
            Renewal::Failed(_) => {}
        }
        true
    }
}

impl WorkspaceService {
    /// The one place the service asks a backend to start an operation. It
    /// is reached only by the call whose `insert_operation` created the
    /// row (INV-3). A dispatch that errs or hangs past the call timeout may
    /// have started: the caller gives up on it. A cancel is not raced here
    /// (abandoning an answer on its way would make the outcome uncertain);
    /// the watch loop delivers it after the ack.
    pub(super) async fn dispatch_once(
        &self,
        handle: &MachineHandle,
        request: &OperationRequest,
    ) -> Result<DispatchAck, String> {
        let timeout = self.config.call_timeout_ms;
        tokio::select! {
            biased;
            ack = self.backend.dispatch(handle, request) => {
                ack.map_err(|error| format!("dispatch failed: {error}"))
            }
            () = self.pause.pause(timeout) => Err(format!("dispatch did not answer within {timeout} ms")),
        }
    }

    /// Poll until a receipt, a give-up or a lost lease. With
    /// `cancel_if_running` a still-running operation is cancelled first
    /// (adopting one whose watcher is gone). The quit cancels like `cancel`.
    /// Once a cancel was sent the token no longer shortens a nap.
    pub(super) async fn watch(
        &self,
        mut w: Watch<'_>,
        cancel: &CancellationToken,
        cancel_if_running: bool,
    ) -> Finished {
        let Some(handle) = w.handle.clone() else {
            return self.give_up(w, "no machine serves the operation").await;
        };
        loop {
            if let Err(end) = self.keep_lease(&mut w).await {
                return match end {
                    None => w.lost(),
                    Some(reason) => self.give_up(w, &reason).await,
                };
            }
            if let Err(reason) = self.cancel_if_asked(&mut w, &handle, cancel).await {
                return self.give_up(w, &reason).await;
            }
            let call = self.backend.observe(&handle, &w.op.id, w.seq);
            let seen = match self.bounded("observe", call).await {
                Ok(seen) => seen,
                Err(error) => return self.give_up(w, &format!("observe failed: {error}")).await,
            };
            w.absorb(&seen);
            if seen.state.is_terminal() && seen.eof {
                return self.settle(w, seen.state).await;
            }
            if seen.state == OperationState::Unknown {
                let reason = "the guest has no record of the operation";
                return self.give_up(w, reason).await;
            }
            let running = !seen.state.is_terminal();
            if running && cancel_if_running && w.cancel_at.is_none() {
                if let Err(reason) = self.send_cancel(&mut w, &handle).await {
                    return self.give_up(w, &reason).await;
                }
                continue;
            }
            if running && w.cancel_at.is_some_and(|at| self.now() >= at) {
                let reason = "cancelled, and no receipt arrived within the grace";
                return self.give_up(w, reason).await;
            }
            if seen.output.is_empty() {
                let wake = w.cancel_at.is_none().then_some(cancel);
                self.nap(self.config.poll_interval_ms, wake).await;
            }
        }
    }

    /// Before a step that may wait on the backend: renew the lease when
    /// less than the longest step is left. `Err(None)`: lost;
    /// `Err(Some(reason))`: the store failed.
    async fn keep_lease(&self, w: &mut Watch<'_>) -> Result<(), Option<String>> {
        if !self.lease_due(&w.lease) {
            return Ok(());
        }
        match self.refresh(&w.lease).await {
            Renewal::Kept(lease) => {
                w.lease = lease;
                Ok(())
            }
            Renewal::Lost => Err(None),
            Renewal::Failed(error) => Err(Some(format!(
                "the workspace lease could not be renewed: {error}"
            ))),
        }
    }

    /// Deliver the cancel once, when the caller or the quit asks for it.
    async fn cancel_if_asked(
        &self,
        w: &mut Watch<'_>,
        handle: &MachineHandle,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let asked = cancel.is_cancelled() || self.gate.quitting();
        if w.cancel_at.is_some() || !asked {
            return Ok(());
        }
        w.quit = self.gate.quitting();
        self.send_cancel(w, handle).await
    }

    async fn send_cancel(&self, w: &mut Watch<'_>, handle: &MachineHandle) -> Result<(), String> {
        let call = self.backend.cancel(handle, &w.op.id);
        match self.bounded("cancel", call).await {
            Ok(()) => {
                let grace = i64::try_from(self.config.cancel_grace_ms).unwrap_or(i64::MAX);
                w.cancel_at = Some(self.now().saturating_add(grace));
                Ok(())
            }
            Err(error) => Err(format!("the cancel could not be delivered: {error}")),
        }
    }

    /// Write the receipt once, ready the workspace, collect denials.
    async fn settle(&self, mut w: Watch<'_>, state: OperationState) -> Finished {
        if !w.refresh(self).await {
            return w.lost();
        }
        let now = self.now();
        let phase = if w.quit {
            OperationPhase::Interrupted(state)
        } else {
            OperationPhase::Terminal(state)
        };
        let update = OperationUpdate {
            phase,
            ended_at: Some(now),
            log_path: w.op.log_path.clone(),
            truncated: w.log_truncated,
        };
        match self
            .repo
            .update_operation(&w.lease, &w.op.id, &update, now)
            .await
        {
            Ok(_) => {}
            Err(error) if is_lease_lost(&error) => return w.lost(),
            Err(error) => {
                let reason = format!("the operation ended as {state:?}");
                return w.unrecorded(&reason, &error.to_string(), now);
            }
        }
        // The receipt is durable; a refused `mark_ready` is settled by the
        // next use's `prepare`.
        let _ = self.repo.mark_ready(&w.lease, now).await;
        let denied = self.drain(w.handle.as_ref(), &w.op.id).await;
        self.machines.touch(&w.op.workspace_id, now);
        let settlement = if w.quit {
            Settlement::Interrupted(state)
        } else {
            Settlement::Settled(state)
        };
        w.finish(settlement, denied, Some(now))
    }

    /// Mark the operation uncertain, drain its denials and stop its
    /// machine (fence when the stop is unproven). Never re-runs it.
    pub(super) async fn give_up(&self, mut w: Watch<'_>, reason: &str) -> Finished {
        if !w.refresh(self).await {
            return w.lost();
        }
        let now = self.now();
        let update = OperationUpdate {
            phase: OperationPhase::Uncertain,
            ended_at: Some(now),
            log_path: w.op.log_path.clone(),
            truncated: w.log_truncated,
        };
        match self
            .repo
            .update_operation(&w.lease, &w.op.id, &update, now)
            .await
        {
            Ok(_) => {}
            Err(error) if is_lease_lost(&error) => return w.lost(),
            Err(error) => return w.unrecorded(reason, &error.to_string(), now),
        }
        let denied = self.drain(w.handle.as_ref(), &w.op.id).await;
        let (reason, fenced) = match self.stop_after_uncertain(&mut w).await {
            AfterStop::Stopped => (
                format!("{reason}; the machine was stopped and the outcome is unknown"),
                false,
            ),
            AfterStop::Fenced(fence) => (format!("{reason}; {fence}"), true),
            AfterStop::NotFenced(why) => (format!("{reason}; {why}"), false),
        };
        w.finish(Settlement::Uncertain { reason, fenced }, denied, Some(now))
    }

    /// Stop the machine of an uncertain operation's generation. A proven
    /// stop leaves the workspace `Busy` with the uncertain row (the next
    /// use advances the generation); otherwise the workspace is fenced,
    /// under a lease renewed right before the write.
    async fn stop_after_uncertain(&self, w: &mut Watch<'_>) -> AfterStop {
        let op = w.op;
        let reason = match self.stop_proof(&op.workspace_id, op.generation).await {
            StopProof::Stopped => {
                self.machines.forget(&op.workspace_id);
                return AfterStop::Stopped;
            }
            StopProof::Unproven { reason } => reason,
        };
        let fence = format!(
            "operation {} has an unknown outcome and the machine of generation {} could not be proven stopped: {reason}",
            op.id, op.generation
        );
        let unfenced = |error: String| {
            AfterStop::NotFenced(format!(
                "the machine could not be proven stopped and the fence could not be written ({error}); the next use proves the stop again"
            ))
        };
        if !w.refresh(self).await {
            return unfenced("the workspace lease was lost".into());
        }
        match self.fence_with(&w.lease, &fence, op.generation).await {
            SandboxError::Fenced { reason } => AfterStop::Fenced(reason),
            error => unfenced(error.to_string()),
        }
    }

    /// Take over an open operation whose watcher is gone: observe it on
    /// its machine; a receipt settles it, a running one is cancelled
    /// (`cancel_if_running`) and observed, no answer makes it uncertain.
    /// Never dispatches.
    pub(super) async fn adopt(
        &self,
        lease: &LeaseToken,
        op: &Operation,
        cancel_if_running: bool,
    ) -> Result<Finished, SandboxError> {
        self.adopt_with(lease, op, &CancellationToken::new(), cancel_if_running)
            .await
    }

    pub(super) async fn adopt_with(
        &self,
        lease: &LeaseToken,
        op: &Operation,
        cancel: &CancellationToken,
        cancel_if_running: bool,
    ) -> Result<Finished, SandboxError> {
        let mut w = Watch::new(self, op, None, lease);
        let workspace = self.load(&op.workspace_id).await?;
        if workspace.generation != op.generation {
            let reason = "the operation's generation is no longer current";
            return Ok(self.give_up(w, reason).await);
        }
        match self.ensure(&workspace).await {
            Ok(handle) => w.handle = Some(handle),
            Err(error) => {
                let reason = format!("the operation's machine could not be reached: {error}");
                return Ok(self.give_up(w, &reason).await);
            }
        }
        Ok(self.watch(w, cancel, cancel_if_running).await)
    }
}
