//! Machine-side types of the backend port: what to start, the opaque
//! handle, observations, bounded reads and stop proofs.

use serde::{Deserialize, Serialize};

use crate::ids::{Generation, WorkspaceId};
use crate::limits::ResourceLimits;
use crate::network::NetworkPolicy;
use crate::operation::OperationState;
use crate::path::{PathRoot, RelPath};

/// What a backend can do. Read once at composition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendDescriptor {
    /// Backend family, for example `fake`, `openshell` or `container`.
    pub kind: String,
    /// True when the backend can still report an operation's receipt for a
    /// generation after the host lost contact (F-4). Without it, a lost ack
    /// is always uncertain.
    pub supports_status_after_host_loss: bool,
    /// True when the disk quota is a hard limit of the storage; false when
    /// the service must check usage before each mutating operation.
    pub enforces_disk_quota: bool,
}

/// What to start for one workspace generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineSpec {
    /// Workspace whose volume the machine mounts.
    pub workspace_id: WorkspaceId,
    /// Generation the machine serves.
    pub generation: Generation,
    /// Root image, by digest.
    pub image_digest: String,
    /// Resources.
    pub limits: ResourceLimits,
    /// Egress policy at start.
    pub network: NetworkPolicy,
}

/// A live machine as a backend names it. Never persisted as truth.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MachineHandle {
    /// Workspace it serves.
    pub workspace_id: WorkspaceId,
    /// Generation it serves.
    pub generation: Generation,
    /// Backend-private handle text.
    pub opaque: String,
}

/// The backend's acknowledgement of a dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchAck {
    /// Guest state right after the dispatch: `Accepted` or `Running` for a
    /// new operation, the existing state for a repeated one.
    pub state: OperationState,
    /// True when the id was already known with the same argument hash, so
    /// nothing new was started.
    pub existing: bool,
}

/// Status and a bounded slice of output after a sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// Guest state.
    pub state: OperationState,
    /// Output bytes after the requested sequence, bounded by the backend.
    pub output: Vec<u8>,
    /// Sequence to pass to the next observe.
    pub next_seq: u64,
    /// True when the guest dropped output (log limit reached).
    pub truncated: bool,
    /// True when no more output will follow.
    pub eof: bool,
}

/// A bounded, non-mutating read of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFile {
    /// The tree `path` is below.
    pub root: PathRoot,
    /// File below `root`.
    pub path: RelPath,
    /// Byte offset to start at.
    pub offset: u64,
    /// Most bytes to return.
    pub limit_bytes: u64,
    /// Refuse with `file_too_large` when the file is larger.
    pub max_file_bytes: u64,
    /// Return the bytes even when the content is not text (export reads).
    /// Without it a binary file answers `binary` and no bytes.
    pub raw: bool,
}

/// Part of a file and what is known about it. A read may run while a
/// command writes the file, so size and time describe this read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    /// File size in bytes.
    pub size: u64,
    /// Modification time, epoch milliseconds.
    pub mtime_ms: i64,
    /// True when the content is not text; `bytes` is then empty unless the
    /// read asked for `raw` bytes.
    pub binary: bool,
    /// The bytes read.
    pub bytes: Vec<u8>,
    /// True when the read reached the end of the file.
    pub eof: bool,
}

/// A bounded, non-mutating directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntries {
    /// The tree `path` is below.
    pub root: PathRoot,
    /// Directory below `root`.
    pub path: RelPath,
    /// Depth, `LIST_DEPTH_MIN..=LIST_DEPTH_MAX`.
    pub depth: u8,
    /// Most entries to return.
    pub max_entries: u32,
}

/// Kind of a listed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link (never followed).
    Symlink,
    /// Anything else.
    Other,
}

/// One listed entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListEntry {
    /// Path below the listing's root.
    pub path: RelPath,
    /// Kind.
    pub kind: EntryKind,
    /// Size in bytes (files only).
    pub size: Option<u64>,
}

/// A directory listing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// Entries, at most the requested count.
    pub entries: Vec<ListEntry>,
    /// True when entries were left out.
    pub truncated: bool,
}

/// Whether the machine of a generation is proven stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof", rename_all = "snake_case")]
pub enum StopProof {
    /// No process of that generation runs and none can write its volume.
    Stopped,
    /// The backend cannot prove it; the workspace must be fenced (INV-5).
    Unproven {
        /// Why, for the owner.
        reason: String,
    },
}

/// Disk use of one workspace volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsage {
    /// Bytes in use.
    pub used_bytes: u64,
    /// Quota in bytes.
    pub quota_bytes: u64,
}

impl StorageUsage {
    /// True when no space is left under the quota.
    pub fn is_exhausted(&self) -> bool {
        self.used_bytes >= self.quota_bytes
    }
}
