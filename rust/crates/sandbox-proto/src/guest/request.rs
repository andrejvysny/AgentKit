//! Host-to-guest requests.

use serde::{Deserialize, Serialize};

use super::{OutputLimit, PathRoot};

/// A request from the host. Tagged by `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// First message on a channel. The guest checks `protocol` against
    /// [`crate::PROTOCOL_VERSION`] and `generation` against its own and
    /// answers [`super::Reply::Hello`] or closes.
    Hello {
        /// The host's protocol version.
        protocol: u32,
        /// The generation the host believes the machine serves.
        generation: u64,
    },
    /// Start a command. Answers `accepted` or `conflict`.
    Exec(ExecRequest),
    /// Replace a file. `size` bytes follow as a data stream with this
    /// envelope's id; the guest answers `accepted` (or `conflict`, or an
    /// error) after the stream ends.
    Write(WriteRequest),
    /// The state of an operation. Answers `state` or `unknown`.
    Status {
        /// Operation id.
        op: String,
    },
    /// Output of an operation after a log offset. Answers `output_data`
    /// then its data stream, or `unknown`.
    Output {
        /// Operation id.
        op: String,
        /// Log byte offset to read from (the previous `next_seq`).
        after_seq: u64,
        /// Most bytes to return.
        max_bytes: u64,
    },
    /// Cancel a running operation. Answers `state` or `unknown`.
    Cancel {
        /// Operation id.
        op: String,
    },
    /// Read part of a file. Answers `file_data` then its data stream.
    Read(ReadRequest),
    /// List a directory. Answers `entries`.
    List(ListRequest),
    /// Bytes used under both roots. Answers `usage`.
    Usage,
    /// End every process, sync, answer `bye` and exit.
    Shutdown,
}

/// Run shell text through the image's non-login shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Operation id (the ledger's `execution_id`).
    pub op: String,
    /// The ledger's argument hash, 64 lowercase hex characters; the same
    /// `op` with another hash is a conflict.
    pub args_sha256: String,
    /// Shell text.
    pub command: String,
    /// Working directory below `/workspace`; created when missing.
    pub cwd: String,
    /// Time the command may run, in milliseconds from its start; a
    /// duration, not a clock time, so host and guest clocks may differ.
    pub timeout_ms: u64,
    /// Output kept in the log before the command ends as `output_limit`.
    pub output_limit: OutputLimit,
    /// `TZ` for the command, when set.
    pub tz: Option<String>,
}

/// Atomically replace one file, creating parent directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteRequest {
    /// Operation id.
    pub op: String,
    /// The ledger's argument hash.
    pub args_sha256: String,
    /// The tree `path` is below.
    pub root: PathRoot,
    /// Target below `root`.
    pub path: String,
    /// SHA-256 of the content, 64 lowercase hex characters; checked before
    /// the rename.
    pub sha256: String,
    /// Content length; the data stream carries exactly this many bytes.
    pub size: u64,
    /// Permission bits, at most `0o777`.
    pub mode: u32,
}

/// A bounded read of one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadRequest {
    /// The tree `path` is below.
    pub root: PathRoot,
    /// File below `root`.
    pub path: String,
    /// Byte offset to start at.
    pub offset: u64,
    /// Most bytes to return.
    pub limit_bytes: u64,
    /// Refuse with `file_too_large` when the file is larger.
    pub max_file_bytes: u64,
    /// Return the bytes even when the content is not text.
    pub raw: bool,
}

/// A bounded directory listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRequest {
    /// The tree `path` is below.
    pub root: PathRoot,
    /// Directory below `root`.
    pub path: String,
    /// Depth, at least 1.
    pub depth: u8,
    /// Most entries to return.
    pub max_entries: u32,
}
