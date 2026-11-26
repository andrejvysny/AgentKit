//! Guest-to-host replies.

use serde::{Deserialize, Serialize};

use super::{Entry, OperationState};

/// A reply from the guest, carrying its request's id. Tagged by `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// Answer to `hello`.
    Hello {
        /// The guest's protocol version.
        protocol: u32,
        /// The generation the guest serves.
        generation: u64,
        /// Digest of the image the guest runs; the host checks it.
        image_digest: String,
        /// The per-generation channel token the launcher gave the guest in
        /// its environment. The host refuses a peer that does not know it,
        /// so a process that reaches the control socket first cannot pose
        /// as the guest.
        channel_token: String,
    },
    /// Answer to `exec` or `write`.
    Accepted {
        /// State right after the request: `accepted` or `running` for a new
        /// operation, the recorded state for a repeated one.
        state: OperationState,
        /// True when the id was already known with the same argument hash,
        /// so nothing new was started.
        existing: bool,
    },
    /// The operation id is known with another argument hash.
    Conflict,
    /// Answer to `status` or `cancel`.
    State {
        /// The operation's state.
        state: OperationState,
    },
    /// The guest has no record of the operation id in this generation.
    Unknown,
    /// Answer to `read`; `len` bytes follow as a data stream.
    FileData(FileData),
    /// Answer to `output`; `len` bytes follow as a data stream.
    OutputData(OutputData),
    /// Answer to `list`.
    Entries {
        /// Entries, at most the requested count.
        entries: Vec<Entry>,
        /// True when entries were left out.
        truncated: bool,
    },
    /// Answer to `usage`.
    Usage {
        /// Bytes used under `/workspace`.
        workspace_bytes: u64,
        /// Bytes used under `/home/agent`.
        home_bytes: u64,
        /// False when part of either tree could not be measured (an
        /// unreadable directory, a skipped name, a depth or count limit).
        /// The host must then not trust the figures for a quota check.
        complete: bool,
    },
    /// Answer to `shutdown`; the guest exits after it.
    Bye,
    /// The request failed. The code's fields sit beside `code` and
    /// `message`, for example
    /// `{ "type": "error", "code": "file_too_large", "size": 9, "message": "…" }`.
    Error(GuestError),
}

/// Header of a file read. A read may run while a command writes the file,
/// so size and time describe this read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileData {
    /// File size in bytes.
    pub size: u64,
    /// Modification time, epoch milliseconds.
    pub mtime_ms: i64,
    /// True when the content is not text; no bytes follow then unless the
    /// read asked for `raw`.
    pub binary: bool,
    /// True when the read reached the end of the file.
    pub eof: bool,
    /// Bytes in the data stream that follows.
    pub len: u64,
}

/// Header of an output read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputData {
    /// The operation's state.
    pub state: OperationState,
    /// Log byte offset to pass as the next `after_seq`.
    pub next_seq: u64,
    /// True when the guest dropped output (the log limit was reached).
    pub truncated: bool,
    /// True when no more output will follow.
    pub eof: bool,
    /// Bytes in the data stream that follows.
    pub len: u64,
}

/// A failed request: a closed code and text for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestError {
    /// What failed; its fields are flattened beside `code`.
    #[serde(flatten)]
    pub code: GuestErrorCode,
    /// Detail for logs; never a discriminant.
    pub message: String,
}

/// Why the guest refused or failed a request. Tagged by `"code"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum GuestErrorCode {
    /// The path does not exist.
    NotFound,
    /// A path component, or the listed path, is not a directory.
    NotADirectory,
    /// The read target is a directory, link or special file.
    NotARegularFile,
    /// The file is larger than the request's `max_file_bytes`.
    FileTooLarge {
        /// The file's size.
        size: u64,
    },
    /// Written bytes do not match the request's `sha256`.
    DigestMismatch,
    /// The data stream did not carry the request's `size` bytes.
    SizeMismatch,
    /// The path is absolute, escapes its root or crosses a link.
    InvalidPath,
    /// The request is malformed or out of bounds.
    InvalidRequest,
    /// `hello` named another protocol version.
    ProtocolMismatch,
    /// `hello` named another generation.
    GenerationMismatch,
    /// The volume is full.
    NoSpace,
    /// The guest cannot take the request now.
    Busy,
    /// Any other failure inside the guest.
    Internal,
}
