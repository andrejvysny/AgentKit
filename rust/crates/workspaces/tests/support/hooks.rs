//! A backend wrapper with switches the review probes need: hang every call
//! after the next dispatch, hold one policy push or one machine start at a
//! gate, prove a never-started generation stopped without telling the
//! backend below (a backend that does not enforce its own proofs), fail
//! the next storage destroy, and answer every file read as stale.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};

use agentkit_workspaces::{
    BackendDescriptor, DenialBatch, DispatchAck, FileChunk, Generation, ListEntries, Listing,
    MachineHandle, MachineSpec, NetworkPolicy, Observation, OperationId, OperationRequest,
    ReadFile, SandboxBackend, SandboxError, StopProof, StorageUsage, WorkspaceId,
};
use async_trait::async_trait;
use tokio::sync::Notify;

/// Switches over an inner backend. Every switch is off by default.
pub struct Hooked {
    inner: Arc<dyn SandboxBackend>,
    /// After the next dispatch, observe, cancel, drain and stop never answer.
    pub hang_after_dispatch: AtomicBool,
    hanging: AtomicBool,
    /// The next policy push waits at the gate.
    pub gate_policy: AtomicBool,
    /// The next start of this generation waits at the gate (0: none).
    pub gate_ensure: AtomicU64,
    /// A stop the inner backend cannot prove for lack of a record answers
    /// `Stopped` here; the inner backend is not told.
    pub absent_proves: AtomicBool,
    /// The next `destroy_storage` fails.
    pub fail_destroy: AtomicBool,
    /// Every `read_file` answers a stale generation.
    pub stale_reads: AtomicBool,
    /// `read_file` calls so far.
    pub reads: AtomicU64,
    /// Notified when a gated call arrives.
    pub entered: Notify,
    /// Lets a gated call go on.
    pub release: Notify,
}

impl Hooked {
    pub fn new(inner: Arc<dyn SandboxBackend>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            hang_after_dispatch: AtomicBool::new(false),
            hanging: AtomicBool::new(false),
            gate_policy: AtomicBool::new(false),
            gate_ensure: AtomicU64::new(0),
            absent_proves: AtomicBool::new(false),
            fail_destroy: AtomicBool::new(false),
            stale_reads: AtomicBool::new(false),
            reads: AtomicU64::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }

    /// Calls answer again.
    pub fn stop_hanging(&self) {
        self.hanging.store(false, SeqCst);
    }

    async fn maybe_hang(&self) {
        if self.hanging.load(SeqCst) {
            std::future::pending::<()>().await;
        }
    }

    async fn gate(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

#[async_trait]
impl SandboxBackend for Hooked {
    fn describe(&self) -> BackendDescriptor {
        self.inner.describe()
    }

    async fn ensure_machine(&self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError> {
        let generation = spec.generation.0;
        if self
            .gate_ensure
            .compare_exchange(generation, 0, SeqCst, SeqCst)
            .is_ok()
        {
            self.gate().await;
        }
        self.inner.ensure_machine(spec).await
    }

    async fn dispatch(
        &self,
        m: &MachineHandle,
        op: &OperationRequest,
    ) -> Result<DispatchAck, SandboxError> {
        let ack = self.inner.dispatch(m, op).await;
        if self.hang_after_dispatch.swap(false, SeqCst) {
            self.hanging.store(true, SeqCst);
        }
        ack
    }

    async fn observe(
        &self,
        m: &MachineHandle,
        id: &OperationId,
        after_seq: u64,
    ) -> Result<Observation, SandboxError> {
        self.maybe_hang().await;
        self.inner.observe(m, id, after_seq).await
    }

    async fn cancel(&self, m: &MachineHandle, id: &OperationId) -> Result<(), SandboxError> {
        self.maybe_hang().await;
        self.inner.cancel(m, id).await
    }

    async fn read_file(
        &self,
        m: &MachineHandle,
        req: &ReadFile,
    ) -> Result<FileChunk, SandboxError> {
        self.reads.fetch_add(1, SeqCst);
        if self.stale_reads.load(SeqCst) {
            return Err(SandboxError::StaleGeneration {
                requested: m.generation,
                current: m.generation,
            });
        }
        self.inner.read_file(m, req).await
    }

    async fn list(&self, m: &MachineHandle, req: &ListEntries) -> Result<Listing, SandboxError> {
        self.inner.list(m, req).await
    }

    async fn stop_machine(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> Result<StopProof, SandboxError> {
        self.maybe_hang().await;
        match self.inner.stop_machine(ws, generation).await? {
            StopProof::Unproven { reason }
                if reason.contains("no record") && self.absent_proves.load(SeqCst) =>
            {
                Ok(StopProof::Stopped)
            }
            other => Ok(other),
        }
    }

    async fn apply_network_policy(
        &self,
        m: &MachineHandle,
        p: &NetworkPolicy,
    ) -> Result<(), SandboxError> {
        if self.gate_policy.swap(false, SeqCst) {
            self.gate().await;
        }
        self.inner.apply_network_policy(m, p).await
    }

    async fn drain_denials(
        &self,
        m: &MachineHandle,
        cursor: u64,
    ) -> Result<DenialBatch, SandboxError> {
        self.maybe_hang().await;
        self.inner.drain_denials(m, cursor).await
    }

    async fn storage_usage(&self, ws: &WorkspaceId) -> Result<StorageUsage, SandboxError> {
        self.inner.storage_usage(ws).await
    }

    async fn reset_home(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        self.inner.reset_home(ws).await
    }

    async fn destroy_storage(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        if self.fail_destroy.swap(false, SeqCst) {
            return Err(SandboxError::Backend {
                reason: "injected destroy failure".into(),
            });
        }
        self.inner.destroy_storage(ws).await
    }
}
