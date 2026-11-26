//! Mutating calls: `exec`, `write_file`, `copy_in`. Order (spec "Rules"):
//! admission (closed once the service quits) -> resolve -> a settled or
//! uncertain row of this id is read back, never run again (replay, INV-7)
//! -> refuse a fenced or deleted workspace -> the workspace lease (wait,
//! then `sandbox_busy`; a delivery of an id whose row is open waits for the
//! earlier delivery instead) -> the same id open is adopted, never
//! dispatched -> settle what blocks the workspace and ensure the machine of
//! the current generation -> quota -> network policy -> lease renewed ->
//! the row as `Dispatching` -> lease renewed -> dispatch -> observe until a
//! receipt or an uncertain end -> release the lease.
//!
//! Once `insert_operation` created the row the call answers an outcome,
//! never an error (SBX-07). A future dropped mid-operation (its caller
//! gone) writes nothing more: its lease runs out and the next holder adopts
//! the open row, observing and never dispatching it.

use crate::error::SandboxError;
use crate::lease::LeaseToken;
use crate::limits::{COPY_IN_MAX_BYTES, WRITE_FILE_MAX_BYTES};
use crate::machine::MachineHandle;
use crate::operation::{
    Operation, OperationKind, OperationKindTag, OperationPhase, OperationRequest, OperationUpdate,
};
use crate::path::RelPath;
use crate::workspace::{Workspace, WorkspaceState};

use super::WorkspaceService;
use super::lease::{Renewal, is_lease_lost};
use super::paths;
use super::prepare::state_error;
use super::replay::{Redelivery, outcome, replay};
use super::types::{Binding, ExecRequest, FileRef, OpContext, OperationOutcome, WriteRequest};
use super::watch::Watch;

/// What a call asks for, before it is resolved against a machine.
pub(super) enum Plan {
    Exec(ExecRequest),
    Write(WriteRequest),
    CopyIn { name: RelPath, bytes: Vec<u8> },
}

/// A plan resolved for one workspace generation.
pub(super) struct Staged {
    pub(super) kind: OperationKind,
    pub(super) summary: String,
    pub(super) log_path: Option<RelPath>,
    pub(super) written: Option<FileRef>,
}

impl Plan {
    fn tag(&self) -> OperationKindTag {
        match self {
            Plan::Exec(_) => OperationKindTag::Exec,
            Plan::Write(_) | Plan::CopyIn { .. } => OperationKindTag::WriteFile,
        }
    }

    /// The summary a row of this call must carry, when it is known before
    /// the machine is reached (a copy-in's name depends on the volume).
    fn summary(&self, binding: &Binding) -> Result<Option<String>, SandboxError> {
        Ok(match self {
            Plan::Exec(request) => Some(request.command.clone()),
            Plan::Write(request) => Some(paths::display(&paths::resolve(
                binding.folder.as_ref(),
                &request.file,
            )?)),
            Plan::CopyIn { .. } => None,
        })
    }

    fn new_bytes(&self) -> u64 {
        match self {
            Plan::Exec(_) => 0,
            Plan::Write(request) => request.bytes.len() as u64,
            Plan::CopyIn { bytes, .. } => bytes.len() as u64,
        }
    }
}

impl WorkspaceService {
    /// Run `request` as operation `ctx.operation_id` and wait for its end.
    /// A delivered-again id never runs twice: a settled or uncertain row is
    /// read back, an open one is adopted (observed, cancelled, settled or
    /// made uncertain). `Settlement::Uncertain` is neither success nor
    /// failure.
    pub async fn exec(
        &self,
        ctx: &OpContext,
        binding: &Binding,
        request: ExecRequest,
    ) -> Result<OperationOutcome, SandboxError> {
        self.run(ctx, binding, Plan::Exec(request)).await
    }

    /// Atomically replace one file (at most `WRITE_FILE_MAX_BYTES`), below
    /// the binding's folder or below `/home/agent`.
    pub async fn write_file(
        &self,
        ctx: &OpContext,
        binding: &Binding,
        request: WriteRequest,
    ) -> Result<OperationOutcome, SandboxError> {
        let size = request.bytes.len() as u64;
        if size > WRITE_FILE_MAX_BYTES {
            return Err(SandboxError::FileTooLarge {
                size,
                limit: WRITE_FILE_MAX_BYTES,
            });
        }
        self.run(ctx, binding, Plan::Write(request)).await
    }

    /// Copy user bytes into `<folder>/input/<name>` (a numeric suffix on a
    /// name already taken; at most [`COPY_IN_MAX_BYTES`]) and mark the
    /// workspace as holding user files before the bytes can land (the
    /// disclosure gate, G-9). `outcome.written` names the file.
    pub async fn copy_in(
        &self,
        ctx: &OpContext,
        binding: &Binding,
        name: &str,
        bytes: Vec<u8>,
    ) -> Result<OperationOutcome, SandboxError> {
        let size = bytes.len() as u64;
        if size > COPY_IN_MAX_BYTES {
            return Err(SandboxError::FileTooLarge {
                size,
                limit: COPY_IN_MAX_BYTES,
            });
        }
        let name = paths::file_name(name)?;
        self.run(ctx, binding, Plan::CopyIn { name, bytes }).await
    }

    async fn run(
        &self,
        ctx: &OpContext,
        binding: &Binding,
        plan: Plan,
    ) -> Result<OperationOutcome, SandboxError> {
        let _admitted = self.gate.admit()?;
        let workspace = self.resolve(binding).await?;
        self.check_member(&workspace, &ctx.agent_id).await?;
        let summary = plan.summary(binding)?;
        let known = self
            .existing(ctx, &workspace, &plan, summary.as_deref())
            .await?;
        let fenced = matches!(workspace.state, WorkspaceState::Fenced { .. });
        if let Some(row) = known.as_ref().filter(|row| !row.phase.is_open()) {
            return Ok(replay(row, fenced));
        }
        if fenced || workspace.state == WorkspaceState::Deleted {
            return Err(state_error(&workspace.state, &workspace.id));
        }
        let holder = self.agent_holder(ctx);
        let call = (&plan, summary.as_deref());
        let lease = match self.await_lease(ctx, &workspace, &holder, call).await? {
            Redelivery::Leased(lease) => lease,
            Redelivery::Ended(outcome) => return Ok(outcome),
        };
        let result = self
            .run_leased(ctx, binding, plan, &workspace, summary.as_deref(), &lease)
            .await;
        self.release(&lease).await;
        result
    }

    /// The row of `ctx.operation_id`, after checking it is this call's: the
    /// same workspace, kind, agent, run, argument hash and (when known)
    /// summary.
    pub(super) async fn existing(
        &self,
        ctx: &OpContext,
        workspace: &Workspace,
        plan: &Plan,
        summary: Option<&str>,
    ) -> Result<Option<Operation>, SandboxError> {
        let Some(row) = self.repo.operation(&ctx.operation_id).await? else {
            return Ok(None);
        };
        let same = row.workspace_id == workspace.id
            && row.kind == plan.tag()
            && row.actor_agent_id == ctx.agent_id
            && row.run_id == ctx.run_id
            && row.args_sha256 == ctx.args_sha256
            && summary.is_none_or(|summary| summary == row.summary);
        if !same {
            return Err(SandboxError::OperationConflict(ctx.operation_id.clone()));
        }
        Ok(Some(row))
    }

    async fn run_leased(
        &self,
        ctx: &OpContext,
        binding: &Binding,
        plan: Plan,
        workspace: &Workspace,
        summary: Option<&str>,
        lease: &LeaseToken,
    ) -> Result<OperationOutcome, SandboxError> {
        if let Some(row) = self.existing(ctx, workspace, &plan, summary).await? {
            return self.resume(ctx, lease, &row, None).await;
        }
        let (workspace, handle) = self.prepare(lease).await?;
        self.check_quota(&workspace, plan.new_bytes()).await?;
        if ctx.cancellation.is_cancelled() {
            return Err(SandboxError::Cancelled);
        }
        let marks_user_files = matches!(plan, Plan::CopyIn { .. });
        let folder = binding.folder.as_ref();
        let staged = self.stage(ctx, folder, plan, &handle, lease).await?;
        self.apply_policy(&workspace, &handle).await?;
        // The steps above may have waited long: the commit writes below
        // start on a fresh lease, and a quit stops the call here.
        let lease = &self.renew(lease).await?;
        self.gate.check_open()?;
        if marks_user_files {
            // Before the bytes can land: the gate must never lag the files.
            self.repo
                .set_holds_user_files(lease, true, self.now())
                .await?;
        }
        let row = self.row(ctx, &workspace, &staged);
        if !self
            .repo
            .insert_operation(lease, &row, row.started_at)
            .await?
        {
            // The same operation already has its row: it is never
            // dispatched again (INV-3).
            return self.resume(ctx, lease, &row, staged.written).await;
        }
        // From here on the call answers an outcome, never an error.
        Ok(self.launch(ctx, lease, &handle, &row, staged).await)
    }

    /// This call's own operation already has a row (delivered again, or
    /// settled while this call waited for the lease): read a settled one
    /// back, adopt an open one. Never dispatches.
    async fn resume(
        &self,
        ctx: &OpContext,
        lease: &LeaseToken,
        row: &Operation,
        written: Option<FileRef>,
    ) -> Result<OperationOutcome, SandboxError> {
        let stored = self.repo.operation(&row.id).await?;
        let row = stored.as_ref().unwrap_or(row);
        if !row.phase.is_open() {
            return Ok(self.replay_row(row).await);
        }
        let finished = self.adopt_with(lease, row, &ctx.cancellation, true).await?;
        Ok(outcome(row, finished, written))
    }

    /// Dispatch the row just inserted, then watch it. The lease is renewed
    /// right before the dispatch: a worker that lost it in the meantime
    /// dispatches nothing. What remains is a dispatch that reaches the
    /// backend after the lease ran out (the worker stalled between the
    /// renewal and the call); closing that needs a fence epoch the backend
    /// checks (ADR 0038, not decided).
    async fn launch(
        &self,
        ctx: &OpContext,
        lease: &LeaseToken,
        handle: &MachineHandle,
        row: &Operation,
        staged: Staged,
    ) -> OperationOutcome {
        let request = OperationRequest {
            id: row.id.clone(),
            workspace_id: row.workspace_id.clone(),
            generation: row.generation,
            args_sha256: ctx.args_sha256.clone(),
            kind: staged.kind,
        };
        let lease = match self.refresh(lease).await {
            Renewal::Kept(lease) => lease,
            Renewal::Lost => {
                let w = Watch::new(self, row, Some(handle.clone()), lease);
                return outcome(row, w.lost(), staged.written);
            }
            Renewal::Failed(error) => {
                let w = Watch::new(self, row, Some(handle.clone()), lease);
                let reason = format!("the lease could not be renewed before dispatch: {error}");
                return outcome(row, self.give_up(w, &reason).await, staged.written);
            }
        };
        let w = Watch::new(self, row, Some(handle.clone()), &lease);
        let finished = match self.dispatch_once(handle, &request).await {
            Err(reason) => self.give_up(w, &reason).await,
            Ok(ack) => {
                if !ack.state.is_terminal() && self.mark_running(&lease, row).await {
                    w.lost()
                } else {
                    self.watch(w, &ctx.cancellation, false).await
                }
            }
        };
        outcome(row, finished, staged.written)
    }

    /// Record the ack. Answers true when the lease is lost; any other
    /// refusal leaves the row `Dispatching`, which settles the same way.
    async fn mark_running(&self, lease: &LeaseToken, row: &Operation) -> bool {
        let update = OperationUpdate {
            phase: OperationPhase::Running,
            ended_at: None,
            log_path: None,
            truncated: false,
        };
        let now = self.now();
        match self
            .repo
            .update_operation(lease, &row.id, &update, now)
            .await
        {
            Err(error) => is_lease_lost(&error),
            Ok(_) => false,
        }
    }

    fn row(&self, ctx: &OpContext, workspace: &Workspace, staged: &Staged) -> Operation {
        Operation {
            id: ctx.operation_id.clone(),
            workspace_id: workspace.id.clone(),
            generation: workspace.generation,
            kind: staged.kind.tag(),
            actor_agent_id: ctx.agent_id.clone(),
            run_id: ctx.run_id.clone(),
            phase: OperationPhase::Dispatching,
            started_at: self.now(),
            ended_at: None,
            log_path: staged.log_path.clone(),
            truncated: false,
            image_digest: workspace.image_digest.clone(),
            summary: staged.summary.clone(),
            args_sha256: ctx.args_sha256.clone(),
        }
    }
}
