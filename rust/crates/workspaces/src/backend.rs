//! The backend port: one isolated machine per active workspace generation.
//! Implemented by a scriptable fake in `agentkit-testkit` and by each real
//! backend adapter; one conformance suite runs INV-1..INV-9 on all of them
//! (INV-10 and the containment checks on real backends only).
//!
//! A backend owns no durable truth the service relies on: the generation
//! and the operation identity come from the caller. The service, not the
//! backend, admits, journals and fences.

use async_trait::async_trait;

use crate::error::SandboxError;
use crate::ids::{Generation, OperationId, WorkspaceId};
use crate::machine::{
    BackendDescriptor, DispatchAck, FileChunk, ListEntries, Listing, MachineHandle, MachineSpec,
    Observation, ReadFile, StopProof, StorageUsage,
};
use crate::network::{DenialBatch, NetworkPolicy};
use crate::operation::OperationRequest;

/// An isolated execution backend. Every method that takes a
/// [`MachineHandle`] refuses a handle whose generation is older than the
/// one the backend last served for that workspace with
/// [`SandboxError::StaleGeneration`] (INV-8).
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// What this backend can do. Read once at composition; drives the
    /// capability the host advertises.
    fn describe(&self) -> BackendDescriptor;

    /// Idempotent. Starts a machine for `(workspace, generation)` or returns
    /// the live one. MUST refuse a generation lower than the one it last
    /// served for this workspace (INV-8), and MUST refuse with
    /// [`SandboxError::StaleGeneration`] every generation at or below one
    /// it answered `StopProof::Stopped` for, whether a machine of it ever
    /// started or not: an unleased caller may ask for a generation it read
    /// before a stop proved it and moved the workspace past it (INV-5).
    /// The service calls it only for a generation whose predecessor is
    /// proven stopped.
    async fn ensure_machine(&self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError>;

    /// Hand one mutating operation to the guest. The caller has already had
    /// the call admitted by the durable ledger and written the operation
    /// row as `Dispatching` (INV-1). Idempotent on operation id: the same id
    /// with the same `args_sha256` returns the existing state and starts
    /// nothing; the same id with a different hash is
    /// [`SandboxError::OperationConflict`] (INV-2). A generation mismatch
    /// with `m` is [`SandboxError::StaleGeneration`].
    async fn dispatch(
        &self,
        m: &MachineHandle,
        op: &OperationRequest,
    ) -> Result<DispatchAck, SandboxError>;

    /// Status and output after `after_seq`. Never blocks on completion.
    /// Output per call is bounded by the backend whatever the command
    /// printed (INV-9). An id the guest never saw in this generation is
    /// `OperationState::Unknown`, not an error.
    async fn observe(
        &self,
        m: &MachineHandle,
        id: &OperationId,
        after_seq: u64,
    ) -> Result<Observation, SandboxError>;

    /// Ask the guest to end the operation's whole process tree. Returns
    /// once the request is delivered; the receipt is read with `observe`.
    async fn cancel(&self, m: &MachineHandle, id: &OperationId) -> Result<(), SandboxError>;

    /// Non-mutating read, bounded by `req`. Takes no lease and may run
    /// beside a command; the chunk carries size and modification time.
    async fn read_file(&self, m: &MachineHandle, req: &ReadFile)
    -> Result<FileChunk, SandboxError>;

    /// Non-mutating listing, bounded by `req`. Never follows links.
    async fn list(&self, m: &MachineHandle, req: &ListEntries) -> Result<Listing, SandboxError>;

    /// Stop the machine serving `(ws, generation)` and say whether that is
    /// proven. `Stopped` means no process of that generation runs and none
    /// can write the volume again; anything less is `Unproven` and the
    /// workspace stays fenced (INV-5, INV-6). `Stopped` needs verified
    /// absence: a backend answers it for a generation with no machine only
    /// when it can prove none runs (for example the runtime lists no
    /// container or VM for that workspace). A backend that merely has no
    /// record (after its own restart) answers `Unproven`. Once it answered
    /// `Stopped` for a generation, `ensure_machine` refuses that generation
    /// and every lower one for good.
    async fn stop_machine(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> Result<StopProof, SandboxError>;

    /// Replace the effective egress policy of a live machine without a
    /// restart (NET-6). An empty policy lets nothing leave (INV-10).
    async fn apply_network_policy(
        &self,
        m: &MachineHandle,
        p: &NetworkPolicy,
    ) -> Result<(), SandboxError>;

    /// Denied attempts recorded after `cursor` (NET-5). The service turns
    /// them into pending requests and passes the returned cursor next time.
    async fn drain_denials(
        &self,
        m: &MachineHandle,
        cursor: u64,
    ) -> Result<DenialBatch, SandboxError>;

    /// Disk use and quota of the workspace volume.
    async fn storage_usage(&self, ws: &WorkspaceId) -> Result<StorageUsage, SandboxError>;

    /// Empty `/home/agent` and leave `/workspace` alone. The machine must be
    /// stopped; a backend refuses while one runs.
    async fn reset_home(&self, ws: &WorkspaceId) -> Result<(), SandboxError>;

    /// Destroy the workspace volume. The machine must be stopped; a backend
    /// refuses while one runs.
    async fn destroy_storage(&self, ws: &WorkspaceId) -> Result<(), SandboxError>;
}
