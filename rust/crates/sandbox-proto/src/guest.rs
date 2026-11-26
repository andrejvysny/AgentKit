//! Messages between the host (the backend adapter) and the guest
//! supervisor (`agentkit-sandboxd`) over the control channel.
//!
//! Wire style: every message is an [`Envelope`] whose body is internally
//! tagged by `"type"`; type names and field names are `snake_case`. Paths
//! are relative to their [`PathRoot`] and sent as text; ids and digests are
//! text; numbers are plain JSON numbers. These are wire types only: the
//! adapter maps them to the `agentkit-workspaces` port types and validates
//! every value, and the guest validates every request, because neither end
//! trusts the other.
//!
//! Sequence numbers: `after_seq` and `next_seq` of `output` are byte
//! offsets in the operation's log. The `seq` of a data frame is the index
//! of that frame in its stream (see [`crate::stream`]).

use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;

mod reply;
mod request;

pub use reply::{FileData, GuestError, GuestErrorCode, OutputData, Reply};
pub use request::{ExecRequest, ListRequest, ReadRequest, Request, WriteRequest};

/// A request with its id, host to guest.
pub type RequestEnvelope = Envelope<Request>;
/// A reply with its request's id, guest to host.
pub type ReplyEnvelope = Envelope<Reply>;

/// The tree inside the guest a path is resolved against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathRoot {
    /// `/workspace`.
    #[default]
    Workspace,
    /// `/home/agent`.
    Home,
}

/// A cap on output: bytes and optionally lines; the first one reached
/// wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputLimit {
    /// Maximum bytes.
    pub max_bytes: u64,
    /// Maximum lines; `None` means bytes only.
    pub max_lines: Option<u64>,
}

/// The guest's state of one operation. Tagged by `"kind"`, for example
/// `{ "kind": "failed", "exit": null, "signal": 9 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationState {
    /// Recorded; not started.
    Accepted,
    /// The process tree is alive.
    Running,
    /// Exited with status 0 (a write that completed reports `exit: 0`).
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
    /// Ended at its timeout.
    TimedOut,
    /// Ended by a `cancel` request.
    Cancelled,
    /// Ended because its output passed the output limit.
    OutputLimit,
    /// No record of this id in this generation. A `status` of an unknown
    /// id answers the [`Reply::Unknown`] message; this state exists so a
    /// header that embeds a state can say the same.
    Unknown,
}

/// Kind of a listed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Directory,
    /// Symbolic link (listed, never followed).
    Symlink,
    /// Anything else.
    Other,
}

/// One listed entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Path below the listing's root (not below the listed directory).
    pub path: String,
    /// Kind.
    pub kind: EntryKind,
    /// Size in bytes, for files only.
    pub size: Option<u64>,
}

#[cfg(test)]
mod tests;
