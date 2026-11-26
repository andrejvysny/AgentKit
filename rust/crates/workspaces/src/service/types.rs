//! Inputs and results of the workspace service.

use tokio_util::sync::CancellationToken;

use crate::ids::{AgentId, Generation, OperationId, Sha256, WorkspaceId};
use crate::lease::LeaseHolder;
use crate::limits::OutputLimit;
use crate::machine::StorageUsage;
use crate::network::{HostPort, NetworkGrant, NetworkRequest};
use crate::operation::{FileMode, Operation, OperationState};
use crate::path::{PathRoot, RelPath};
use crate::workspace::{Workspace, WorkspaceKey, WorkspaceKind};

/// A run's pinned attachment (D-24): which workspace, and the folder its
/// relative paths resolve in. The host computes it at admission; it never
/// comes from model arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The workspace key, for example `agent:<id>` or `chats:project:<id>`.
    pub key: WorkspaceKey,
    /// What the workspace serves; a stored record of another kind refuses.
    pub kind: WorkspaceKind,
    /// The owner scope; a stored record of another scope refuses.
    pub owner_scope: String,
    /// Working folder below `/workspace` (`chats/<chat id>` in a project's
    /// chat workspace); `None` for the root (agent homes, isolated and
    /// shared workspaces).
    pub folder: Option<RelPath>,
}

/// The identity of one mutating call, as the tool layer passes it after
/// the durable ledger admitted it.
#[derive(Debug, Clone)]
pub struct OpContext {
    /// The `execution_id` the ledger granted (INV-2). Never derived here.
    pub operation_id: OperationId,
    /// The ledger's argument hash; a backend refuses the same id with
    /// another hash.
    pub args_sha256: Sha256,
    /// The run that made the call.
    pub run_id: String,
    /// The agent that made the call (attribution, membership, lease
    /// holder).
    pub agent_id: AgentId,
    /// The tool call's cancellation: before dispatch nothing starts; after
    /// it the service cancels and waits up to the grace for a receipt.
    pub cancellation: CancellationToken,
}

impl OpContext {
    /// A context with a cancellation token of its own.
    pub fn new(
        operation_id: OperationId,
        args_sha256: Sha256,
        run_id: impl Into<String>,
        agent_id: AgentId,
    ) -> Self {
        Self {
            operation_id,
            args_sha256,
            run_id: run_id.into(),
            agent_id,
            cancellation: CancellationToken::new(),
        }
    }
}

/// A shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// Shell text.
    pub command: String,
    /// Directory relative to the binding's folder; `None` is the folder.
    pub directory: Option<RelPath>,
    /// Deadline; `None` is the 120 s default, larger values are cut to
    /// 3,600 s.
    pub timeout_ms: Option<u64>,
    /// What the guest keeps in the log before it ends the command as
    /// `OutputLimit` (default [`OutputLimit::LOG`]); larger byte counts
    /// are cut to it.
    pub output_limit: OutputLimit,
}

impl ExecRequest {
    /// `command` in the binding's folder with the default deadline and log.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            directory: None,
            timeout_ms: None,
            output_limit: OutputLimit::LOG,
        }
    }
}

/// An atomic file write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequest {
    /// The file, relative to the binding's folder for `PathRoot::Workspace`
    /// and to `/home/agent` for `PathRoot::Home`.
    pub file: FileRef,
    /// Content; at most `WRITE_FILE_MAX_BYTES`.
    pub bytes: Vec<u8>,
    /// Mode of the written file.
    pub mode: FileMode,
}

/// A path below one of the guest roots, as a tool names it. Below
/// `/workspace` it is relative to the binding's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// `/workspace` or `/home/agent`.
    pub root: PathRoot,
    /// The path; it can never escape its root.
    pub path: RelPath,
}

impl FileRef {
    /// `path` below the binding's folder in `/workspace`.
    pub fn workspace(path: RelPath) -> Self {
        Self {
            root: PathRoot::Workspace,
            path,
        }
    }
}

/// How an operation ended, as far as the service knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// A receipt: the guest reported how it ended (always terminal).
    Settled(OperationState),
    /// Ended by a desktop quit: the quit cancelled it and this receipt was
    /// read within the cancel grace (always terminal).
    Interrupted(OperationState),
    /// No receipt could be read and the operation may have run. It is
    /// neither success nor failure and is never run again (SBX-07).
    Uncertain {
        /// What happened, for the agent and the owner.
        reason: String,
        /// True when the machine's stop was not proven and the workspace is
        /// now fenced.
        fenced: bool,
    },
}

impl Settlement {
    /// True for [`Settlement::Uncertain`].
    pub fn is_uncertain(&self) -> bool {
        matches!(self, Settlement::Uncertain { .. })
    }
}

/// The result of one mutating operation (command, write or copy-in).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationOutcome {
    /// The ledger's `execution_id`.
    pub operation_id: OperationId,
    /// The workspace it ran in.
    pub workspace_id: WorkspaceId,
    /// The generation it was dispatched to.
    pub generation: Generation,
    /// The image of that generation.
    pub image_digest: String,
    /// How it ended.
    pub settlement: Settlement,
    /// The tail of the output the caller may show: bounded by
    /// `ServiceConfig::model_output`. Empty for writes and replays.
    pub output: Vec<u8>,
    /// True when earlier output was left out of `output`.
    pub output_cut: bool,
    /// True when the guest's log limit was reached.
    pub log_truncated: bool,
    /// The full log below `/workspace` (commands only).
    pub log_path: Option<RelPath>,
    /// The file written below its root (writes and copy-ins).
    pub written: Option<FileRef>,
    /// Connections this operation tried that the policy refused; each one
    /// is recorded as a pending (or rejected) network request.
    pub network_denied: Vec<HostPort>,
    /// True when the operation was not run by this call: a settled or
    /// uncertain row was read back (replay, INV-7).
    pub replayed: bool,
    /// Epoch milliseconds the operation row was written.
    pub started_at: i64,
    /// Epoch milliseconds it settled or became uncertain.
    pub ended_at: Option<i64>,
}

/// A file read whole for an export: bytes, digest and size, bounded by
/// `EXPORT_FILE_MAX_BYTES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFile {
    /// Content.
    pub bytes: Vec<u8>,
    /// Digest of `bytes`; an export approval binds to it.
    pub sha256: Sha256,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, epoch milliseconds.
    pub mtime_ms: i64,
}

/// The operation that holds a workspace, and who holds its lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningOperation {
    /// The open operation row.
    pub operation: Operation,
    /// The current lease holder, when the lease is unexpired.
    pub holder: Option<LeaseHolder>,
}

/// What `sandbox_get_status` shows. A never-used binding has no record
/// and the status says so without creating one (F-16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatus {
    /// The record; `None` when the binding was never used.
    pub workspace: Option<Workspace>,
    /// The working directory inside the guest, for example
    /// `/workspace/chats/c1`.
    pub working_dir: String,
    /// Disk use and quota; `None` when the backend could not answer.
    pub storage: Option<StorageUsage>,
    /// Active network grants.
    pub grants: Vec<NetworkGrant>,
    /// Requests waiting for the owner.
    pub pending_requests: Vec<NetworkRequest>,
    /// The open operation, if any.
    pub running: Option<RunningOperation>,
}

/// The owner's answer to a pending network request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkDecision {
    /// Grant the host and port to the workspace, live.
    Approve,
    /// Refuse; later denials of the target count on this request and the
    /// owner is not asked again.
    Reject,
}

/// The result of stopping a workspace's machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// Proven stopped; the generation advanced, so the next use starts a
    /// fresh machine at `next_generation`.
    Stopped {
        /// The generation now current.
        next_generation: Generation,
    },
    /// No machine of the current generation is known to have started and
    /// the backend has no record of one: nothing was stopped and nothing
    /// changed.
    NotStarted,
    /// The stop is not proven: the workspace is fenced (INV-6).
    Fenced {
        /// Why, for the owner.
        reason: String,
    },
}

/// What startup recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Open operations a receipt settled.
    pub settled: Vec<OperationId>,
    /// Open operations marked uncertain (their machine stopped or fenced).
    pub uncertain: Vec<OperationId>,
    /// Workspaces fenced because a stop was not proven.
    pub fenced: Vec<WorkspaceId>,
    /// Leases of holders that are gone (expired, or of an earlier boot of
    /// this host), broken. A live lease of another host is left alone.
    pub leases_broken: u32,
    /// Workspaces with no open operation whose current generation was
    /// proven stopped and advanced (machines left over from before the
    /// restart).
    pub stopped: Vec<WorkspaceId>,
}

/// What [`super::WorkspaceService::quiesce`] stopped. Each cancelled
/// operation's own call reports how it ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuiesceReport {
    /// Workspaces whose machine was proven stopped.
    pub stopped: Vec<WorkspaceId>,
    /// Workspaces fenced because a stop was not proven.
    pub fenced: Vec<WorkspaceId>,
}
