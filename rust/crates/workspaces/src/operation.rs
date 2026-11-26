//! Mutating operations: what the host asks a backend to do, the guest's
//! view of its state, and the attribution projection the service keeps.
//! The durable execution ledger stays the authority for replay; the
//! projection is for attribution, the activity list and recovery.

use serde::{Deserialize, Serialize};

use crate::ids::{AgentId, Generation, OperationId, Sha256, WorkspaceId};
use crate::limits::OutputLimit;
use crate::path::{PathRoot, RelPath};

/// The guest's state of one operation, as a backend reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OperationState {
    /// The guest recorded the operation; it has not started.
    Accepted,
    /// The process tree is alive.
    Running,
    /// Exited with status 0.
    Succeeded {
        /// Exit status.
        exit: i32,
    },
    /// Exited non-zero or was killed by a signal.
    Failed {
        /// Exit status, when it exited.
        exit: Option<i32>,
        /// Signal number, when one ended it.
        signal: Option<i32>,
    },
    /// The guest ended it at its deadline.
    TimedOut,
    /// Ended by a cancel request.
    Cancelled,
    /// Ended because its output passed the log limit.
    OutputLimit,
    /// The guest has no record of this id in this generation.
    Unknown,
}

impl OperationState {
    /// True for a receipt: the operation ended and will not change again.
    /// `Unknown` is not terminal: it is the absence of a receipt.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            OperationState::Succeeded { .. }
                | OperationState::Failed { .. }
                | OperationState::TimedOut
                | OperationState::Cancelled
                | OperationState::OutputLimit
        )
    }

    /// The exit status, when the operation exited.
    pub fn exit(&self) -> Option<i32> {
        match self {
            OperationState::Succeeded { exit } => Some(*exit),
            OperationState::Failed { exit, .. } => *exit,
            _ => None,
        }
    }
}

/// The service's lifecycle phase of an operation, stored in the projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum OperationPhase {
    /// Written before the backend is asked to dispatch: it may have started.
    Dispatching,
    /// The backend acknowledged it; output is read by sequence.
    Running,
    /// A receipt was read. The inner state is always terminal
    /// ([`OperationPhase::is_valid`]).
    Terminal(OperationState),
    /// No receipt could be read and the operation may have run. Never
    /// re-run; a late receipt may still settle it.
    Uncertain,
    /// Ended by a desktop quit (F-13): the quit cancelled it and its
    /// receipt was read within the grace. The inner state is that receipt
    /// and is always terminal.
    Interrupted(OperationState),
}

impl OperationPhase {
    /// `Terminal` of a terminal state; `None` for a non-terminal one.
    pub fn terminal(state: OperationState) -> Option<OperationPhase> {
        state
            .is_terminal()
            .then_some(OperationPhase::Terminal(state))
    }

    /// False only for a `Terminal` or `Interrupted` holding a non-terminal
    /// state.
    pub fn is_valid(&self) -> bool {
        match self {
            OperationPhase::Terminal(state) | OperationPhase::Interrupted(state) => {
                state.is_terminal()
            }
            _ => true,
        }
    }

    /// True once a receipt settled the operation; it never changes again.
    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            OperationPhase::Terminal(_) | OperationPhase::Interrupted(_)
        )
    }

    /// True while the operation may still be running in the guest under the
    /// service's watch (`Dispatching` or `Running`).
    pub fn is_open(&self) -> bool {
        matches!(self, OperationPhase::Dispatching | OperationPhase::Running)
    }

    /// The transition rule every repository enforces: a settled phase never
    /// changes, `Uncertain` may only be settled by a late receipt, and no
    /// phase returns to `Dispatching`.
    pub fn can_transition_to(&self, next: &OperationPhase) -> bool {
        if !next.is_valid() {
            return false;
        }
        match self {
            OperationPhase::Dispatching => !matches!(next, OperationPhase::Dispatching),
            OperationPhase::Running => !matches!(next, OperationPhase::Dispatching),
            OperationPhase::Uncertain => next.is_settled(),
            OperationPhase::Terminal(_) | OperationPhase::Interrupted(_) => false,
        }
    }

    /// The exit status of a settled operation, when it exited.
    pub fn exit(&self) -> Option<i32> {
        match self {
            OperationPhase::Terminal(state) | OperationPhase::Interrupted(state) => state.exit(),
            _ => None,
        }
    }
}

/// Which kind of mutating operation a projection row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKindTag {
    /// A shell command.
    Exec,
    /// A file write (including a copy-in).
    WriteFile,
}

/// The attribution projection of one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    /// The ledger's `execution_id`.
    pub id: OperationId,
    /// The workspace it ran in.
    pub workspace_id: WorkspaceId,
    /// The machine generation it was dispatched to.
    pub generation: Generation,
    /// Command or write.
    pub kind: OperationKindTag,
    /// The agent that ran it.
    pub actor_agent_id: AgentId,
    /// The run that ran it.
    pub run_id: String,
    /// Service lifecycle phase; `phase.exit()` is the exit status.
    pub phase: OperationPhase,
    /// Epoch milliseconds the row was written (before dispatch).
    pub started_at: i64,
    /// Epoch milliseconds it settled or became uncertain.
    pub ended_at: Option<i64>,
    /// Full command log inside the workspace, under `.sandbox/logs/`.
    pub log_path: Option<RelPath>,
    /// True when output passed a limit.
    pub truncated: bool,
    /// Image digest of the generation it ran on.
    pub image_digest: String,
    /// Command text for an exec, target path for a write.
    pub summary: String,
    /// The ledger's argument hash of the call; the same id with another
    /// hash is a conflict.
    pub args_sha256: Sha256,
}

impl Operation {
    /// True when `other` describes the same operation: every identity field
    /// matches (times, phase and output fields may differ).
    pub fn same_identity(&self, other: &Operation) -> bool {
        self.id == other.id
            && self.workspace_id == other.workspace_id
            && self.generation == other.generation
            && self.kind == other.kind
            && self.actor_agent_id == other.actor_agent_id
            && self.run_id == other.run_id
            && self.summary == other.summary
            && self.args_sha256 == other.args_sha256
    }
}

/// A change to an operation's projection row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationUpdate {
    /// New phase; must pass [`OperationPhase::can_transition_to`].
    pub phase: OperationPhase,
    /// Set when the phase settles or becomes uncertain; `None` keeps the
    /// stored value.
    pub ended_at: Option<i64>,
    /// Log location, once known; `None` keeps the stored value.
    pub log_path: Option<RelPath>,
    /// Output passed a limit; `false` keeps the stored value.
    pub truncated: bool,
}

/// Permission bits of a written file, at most `0o777` (no set-id bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct FileMode(u32);

/// A mode with bits outside `0o777`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("file mode {0:#o} has bits outside 0o777")]
pub struct FileModeError(pub u32);

impl FileMode {
    /// `rw-r--r--`.
    pub const REGULAR: FileMode = FileMode(0o644);
    /// `rwxr-xr-x`.
    pub const EXECUTABLE: FileMode = FileMode(0o755);

    /// Validate mode bits.
    pub fn new(bits: u32) -> Result<Self, FileModeError> {
        if bits & !0o777 == 0 {
            Ok(Self(bits))
        } else {
            Err(FileModeError(bits))
        }
    }

    /// The bits.
    pub fn bits(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for FileMode {
    type Error = FileModeError;

    fn try_from(bits: u32) -> Result<Self, Self::Error> {
        Self::new(bits)
    }
}

impl From<FileMode> for u32 {
    fn from(mode: FileMode) -> Self {
        mode.0
    }
}

/// Where the bytes of a write come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteSource {
    /// The bytes themselves, bounded by the write limit of the caller.
    Inline(Vec<u8>),
}

/// What one mutating operation does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationKind {
    /// Run shell text through the image's non-login shell.
    Exec {
        /// Shell text.
        command: String,
        /// Working directory below `/workspace`. The guest creates it when
        /// missing (a chat's folder at that chat's first use, D-35).
        cwd: RelPath,
        /// Deadline the guest enforces itself.
        timeout_ms: u64,
        /// What the guest keeps of the output before it ends the command.
        output_limit: OutputLimit,
    },
    /// Atomically replace one file, creating parent directories.
    WriteFile {
        /// The tree `path` is below (`/workspace` or `/home/agent`).
        root: PathRoot,
        /// Target below `root`.
        path: RelPath,
        /// Digest of `bytes`; the guest verifies it before the rename.
        sha256: Sha256,
        /// Content.
        bytes: ByteSource,
        /// Mode of the written file.
        mode: FileMode,
    },
}

impl OperationKind {
    /// The projection tag.
    pub fn tag(&self) -> OperationKindTag {
        match self {
            OperationKind::Exec { .. } => OperationKindTag::Exec,
            OperationKind::WriteFile { .. } => OperationKindTag::WriteFile,
        }
    }
}

/// One mutating operation handed to a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRequest {
    /// The ledger's `execution_id` (INV-2).
    pub id: OperationId,
    /// Target workspace.
    pub workspace_id: WorkspaceId,
    /// Generation the caller believes current; a stale one is refused.
    pub generation: Generation,
    /// The ledger's argument hash; the same id with another hash is a
    /// conflict.
    pub args_sha256: Sha256,
    /// What to do.
    pub kind: OperationKind,
}

#[cfg(test)]
mod tests;
