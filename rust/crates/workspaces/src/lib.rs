//! Workspaces context (ADR 0038, layer 3): isolated sandbox workspaces an
//! agent or chat runs commands in. The crate owns the domain types, the two
//! ports it drives (`SandboxBackend` for machines, `WorkspaceRepository` for
//! records, leases, grants and requests) and the [`WorkspaceService`] that
//! drives them. It has no SQL, names no store and no concrete backend. Adapters live in
//! layer 4; the in-memory twin, the fake backend and the conformance suites
//! live in `agentkit-testkit`.
//!
//! Operation identity, replay and the uncertain barrier come from the
//! durable execution ledger in `agentkit-governance`: an [`OperationId`] is
//! the `execution_id` it granted. This crate adds what the ledger lacks:
//! the per-workspace lease, the generation fence and stop proofs.

mod backend;
mod error;
mod ids;
mod lease;
mod limits;
mod machine;
mod network;
mod operation;
mod path;
mod repository;
mod service;
mod workspace;

pub use backend::SandboxBackend;
pub use error::{RepositoryError, SandboxError};
pub use ids::{
    AgentId, Generation, GrantId, OperationId, RequestId, Sha256, Sha256Error, WorkspaceId,
};
pub use lease::{LeaseAcquire, LeaseHolder, LeaseInfo, LeaseToken};
pub use limits::{
    COPY_IN_MAX_BYTES, DENIALS_PER_OPERATION_MAX, EXEC_TIMEOUT_DEFAULT_MS, EXEC_TIMEOUT_MAX_MS,
    EXPORT_FILE_MAX_BYTES, LEASE_WAIT_MS, LIST_DEPTH_MAX, LIST_DEPTH_MIN, LIST_MAX_ENTRIES,
    OBSERVE_CHUNK_MAX_BYTES, OutputLimit, PENDING_REQUESTS_MAX, READ_FILE_MAX_BYTES,
    ResourceLimits, WRITE_FILE_MAX_BYTES,
};
pub use machine::{
    BackendDescriptor, DispatchAck, EntryKind, FileChunk, ListEntries, ListEntry, Listing,
    MachineHandle, MachineSpec, Observation, ReadFile, StopProof, StorageUsage,
};
pub use network::{
    DEFAULT_GRANTS, Denial, DenialBatch, DenialRecord, GrantSource, HOST_MAX_BYTES, HostPort,
    HostPortError, NetworkGrant, NetworkPolicy, NetworkRequest, RequestDecided, RequestDecision,
    RequestState, default_grants,
};
pub use operation::{
    ByteSource, FileMode, FileModeError, Operation, OperationKind, OperationKindTag,
    OperationPhase, OperationRequest, OperationState, OperationUpdate,
};
pub use path::{PathRoot, REL_PATH_MAX_BYTES, REL_PATH_SEGMENT_MAX_BYTES, RelPath, RelPathError};
pub use repository::WorkspaceRepository;
pub use service::{
    Binding, ExecRequest, ExportFile, FileRef, NetworkDecision, OpContext, OperationOutcome, Pause,
    QuiesceReport, RecoveryReport, RunningOperation, ServiceConfig, ServiceConfigError, Settlement,
    StopOutcome, TokioPause, WorkspaceService, WorkspaceStatus, WriteRequest,
};
pub use workspace::{
    Member, MemberRole, NewWorkspace, Workspace, WorkspaceKey, WorkspaceKind, WorkspaceState,
    WorkspaceUpsert,
};
