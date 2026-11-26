//! `SandboxBackend` for the fake. The mutex is never held across an
//! `.await` or a panic.

use agentkit_workspaces::{
    BackendDescriptor, DenialBatch, DispatchAck, FileChunk, Generation, ListEntries, Listing,
    MachineHandle, MachineSpec, NetworkPolicy, Observation, OperationId, OperationKind,
    OperationRequest, OperationState, ReadFile, SandboxBackend, SandboxError, StopProof,
    StorageUsage, WorkspaceId,
};
use async_trait::async_trait;

use super::FakeSandboxBackend;
use super::guest::FakeOp;
use super::program::Program;
use super::state::State;

/// What happens after a new operation was started.
enum Armed {
    Ack,
    LoseAck,
    Hang,
    Panic,
}

impl State {
    /// Validate, replay or start `op`; consume the faults armed for a new
    /// operation.
    fn dispatch(
        &mut self,
        m: &MachineHandle,
        op: &OperationRequest,
        now: i64,
    ) -> Result<(DispatchAck, Armed), SandboxError> {
        *self
            .counters
            .dispatch_calls
            .entry(op.id.clone())
            .or_default() += 1;
        self.machine(m, now)?;
        if op.workspace_id != m.workspace_id {
            return Err(SandboxError::Backend {
                reason: format!("operation {} names another workspace", op.id),
            });
        }
        if op.generation != m.generation {
            return Err(SandboxError::StaleGeneration {
                requested: op.generation,
                current: m.generation,
            });
        }
        if let Some(ack) = self.replay(m, op, now)? {
            return Ok((ack, Armed::Ack));
        }
        let usage = self.usage(&m.workspace_id);
        if self.descriptor.enforces_disk_quota && usage.is_exhausted() {
            return Err(SandboxError::QuotaExceeded {
                used_bytes: usage.used_bytes,
                quota_bytes: usage.quota_bytes,
            });
        }
        let started = FakeOp::new(op, self.program_for(&op.kind), now);
        let state = started.state;
        let die = std::mem::take(&mut self.faults.die_after_next_dispatch);
        let machine = self.machine(m, now)?;
        machine.ops.push(started);
        machine.unreachable |= die;
        let ack = DispatchAck {
            state,
            existing: false,
        };
        Ok((ack, self.take_armed()))
    }

    /// The ack of an id this generation already knows (INV-2).
    fn replay(
        &mut self,
        m: &MachineHandle,
        op: &OperationRequest,
        now: i64,
    ) -> Result<Option<DispatchAck>, SandboxError> {
        let Some(existing) = self.machine(m, now)?.op(&op.id) else {
            return Ok(None);
        };
        if existing.args != op.args_sha256 {
            return Err(SandboxError::OperationConflict(op.id.clone()));
        }
        Ok(Some(DispatchAck {
            state: existing.state,
            existing: true,
        }))
    }

    fn program_for(&mut self, kind: &OperationKind) -> Option<Program> {
        let OperationKind::Exec { command, .. } = kind else {
            return None;
        };
        if std::mem::take(&mut self.faults.endless_next_exec) {
            return Some(Program::endless("y\n"));
        }
        Some(
            self.scripts
                .get(command)
                .cloned()
                .unwrap_or_else(|| Program::unscripted(command)),
        )
    }

    fn take_armed(&mut self) -> Armed {
        let faults = &mut self.faults;
        if std::mem::take(&mut faults.panic_next_dispatch) {
            Armed::Panic
        } else if std::mem::take(&mut faults.hang_next_dispatch) {
            Armed::Hang
        } else if std::mem::take(&mut faults.lose_next_ack) {
            Armed::LoseAck
        } else {
            Armed::Ack
        }
    }
}

#[async_trait]
impl SandboxBackend for FakeSandboxBackend {
    fn describe(&self) -> BackendDescriptor {
        self.lock().descriptor.clone()
    }

    async fn ensure_machine(&self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError> {
        self.lock().ensure(spec)
    }

    async fn dispatch(
        &self,
        m: &MachineHandle,
        op: &OperationRequest,
    ) -> Result<DispatchAck, SandboxError> {
        let now = self.now();
        let outcome = self.lock().dispatch(m, op, now);
        let (ack, armed) = outcome?;
        match armed {
            Armed::Ack => Ok(ack),
            Armed::LoseAck => Err(SandboxError::Backend {
                reason: format!("fake fault: the ack of {} was lost", op.id),
            }),
            Armed::Hang => std::future::pending().await,
            Armed::Panic => panic!("fake fault: dispatch of {} panicked", op.id),
        }
    }

    async fn observe(
        &self,
        m: &MachineHandle,
        id: &OperationId,
        after_seq: u64,
    ) -> Result<Observation, SandboxError> {
        let now = self.now();
        let mut state = self.lock();
        let max = state.observe_max_bytes;
        let machine = state.machine(m, now)?;
        Ok(match machine.op(id) {
            Some(op) => op.observe(after_seq, max),
            None => Observation {
                state: OperationState::Unknown,
                output: Vec::new(),
                next_seq: after_seq,
                truncated: false,
                eof: false,
            },
        })
    }

    async fn cancel(&self, m: &MachineHandle, id: &OperationId) -> Result<(), SandboxError> {
        let now = self.now();
        self.lock().cancel(m, id, now)
    }

    async fn read_file(
        &self,
        m: &MachineHandle,
        req: &ReadFile,
    ) -> Result<FileChunk, SandboxError> {
        let now = self.now();
        let mut state = self.lock();
        state.machine(m, now)?;
        state.workspaces[&m.workspace_id].tree(req.root).read(req)
    }

    async fn list(&self, m: &MachineHandle, req: &ListEntries) -> Result<Listing, SandboxError> {
        let now = self.now();
        let mut state = self.lock();
        state.machine(m, now)?;
        state.workspaces[&m.workspace_id].tree(req.root).list(req)
    }

    async fn stop_machine(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> Result<StopProof, SandboxError> {
        let now = self.now();
        Ok(self.lock().stop(ws, generation, now))
    }

    async fn apply_network_policy(
        &self,
        m: &MachineHandle,
        p: &NetworkPolicy,
    ) -> Result<(), SandboxError> {
        let now = self.now();
        self.lock().machine(m, now)?.policy = p.clone();
        Ok(())
    }

    async fn drain_denials(
        &self,
        m: &MachineHandle,
        cursor: u64,
    ) -> Result<DenialBatch, SandboxError> {
        let now = self.now();
        let mut state = self.lock();
        let denials = &state.machine(m, now)?.denials;
        let start = usize::try_from(cursor)
            .unwrap_or(usize::MAX)
            .min(denials.len());
        Ok(DenialBatch {
            denials: denials[start..].to_vec(),
            cursor: denials.len() as u64,
        })
    }

    async fn storage_usage(&self, ws: &WorkspaceId) -> Result<StorageUsage, SandboxError> {
        let now = self.now();
        let mut state = self.lock();
        state.catch_up(ws, now);
        Ok(state.usage(ws))
    }

    async fn reset_home(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let mut state = self.lock();
        state.require_stopped(ws)?;
        if let Some(rec) = state.workspaces.get_mut(ws) {
            rec.home = Default::default();
            rec.home_resets += 1;
        }
        Ok(())
    }

    async fn destroy_storage(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        let mut state = self.lock();
        state.require_stopped(ws)?;
        if let Some(rec) = state.workspaces.get_mut(ws) {
            rec.volume = Default::default();
            rec.home = Default::default();
            rec.disk_full = false;
        }
        Ok(())
    }
}
