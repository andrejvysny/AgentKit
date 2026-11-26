//! Messages between the backend adapter and the launcher service
//! (`agentkit-sandbox-launcher`). Control frames only, in the same
//! [`Envelope`] and wire style as [`crate::guest`]. The API is closed: no
//! request names an image, mount, device, flag or network. The launcher
//! validates every workspace id with [`crate::valid_workspace_id`] before
//! it builds a path or volume name.

use serde::{Deserialize, Serialize};

use crate::envelope::Envelope;

/// A launcher request with its id.
pub type RequestEnvelope = Envelope<Request>;
/// A launcher reply with its request's id.
pub type ReplyEnvelope = Envelope<Reply>;

/// A request to the launcher. Tagged by `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Start the workspace's container for `generation`, or confirm it
    /// runs. Refused for a generation at or below one proven stopped, and
    /// while a container of another generation runs. Answers `ensured`.
    Ensure {
        /// Workspace id.
        workspace_id: String,
        /// Generation the container serves.
        generation: u64,
        /// Expected image digest; refused unless it is the launcher's own.
        image_digest: String,
        /// Resources.
        tier: Tier,
    },
    /// Stop and remove the container of `generation`. Answers `stopped`.
    Stop {
        /// Workspace id.
        workspace_id: String,
        /// Generation to stop.
        generation: u64,
        /// Grace period before the runtime kills it, in milliseconds.
        grace_ms: u64,
    },
    /// What runs for the workspace. Answers `inspected`.
    Inspect {
        /// Workspace id.
        workspace_id: String,
    },
    /// Bytes used by the workspace's volumes. Answers `usage`.
    Usage {
        /// Workspace id.
        workspace_id: String,
    },
    /// Recreate the home volume. Refused while a container exists.
    /// Answers `done`.
    ResetHome {
        /// Workspace id.
        workspace_id: String,
    },
    /// Remove both volumes. Refused while a container exists. Answers
    /// `done`.
    Destroy {
        /// Workspace id.
        workspace_id: String,
    },
}

/// Container resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tier {
    /// CPU quota in thousandths of a CPU.
    pub cpus_milli: u32,
    /// Memory limit in bytes.
    pub memory_bytes: u64,
    /// Process limit.
    pub pids: u32,
}

/// A reply from the launcher. Tagged by `"type"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// The container of the requested generation runs.
    Ensured {
        /// Runtime container id.
        container_id: String,
        /// The per-generation channel token placed in the container's
        /// environment (`AGENTKIT_CHANNEL_TOKEN`); the guest answers it in
        /// `hello`.
        channel_token: String,
    },
    /// Answer to `stop`.
    Stopped {
        /// True only when the runtime lists no container for that
        /// workspace and generation.
        proven: bool,
        /// Why it is not proven.
        reason: Option<String>,
    },
    /// Answer to `inspect`.
    Inspected {
        /// Generation of the running container, if any.
        running: Option<u64>,
        /// Highest generation proven stopped, if any.
        stopped_through: Option<u64>,
    },
    /// Answer to `usage`.
    Usage {
        /// Bytes used by the workspace volume.
        workspace_bytes: u64,
        /// Bytes used by the home volume.
        home_bytes: u64,
    },
    /// The request completed.
    Done,
    /// The request was refused or failed. The code's fields sit beside
    /// `code` and `message`, for example
    /// `{ "type": "error", "code": "stale_generation", "current": 4, "message": "…" }`.
    Error(LauncherError),
}

/// A refused or failed launcher request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LauncherError {
    /// What failed; its fields are flattened beside `code`.
    #[serde(flatten)]
    pub code: LauncherErrorCode,
    /// Detail for logs; never a discriminant.
    pub message: String,
}

/// Why the launcher refused or failed a request. Tagged by `"code"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum LauncherErrorCode {
    /// The generation is at or below one proven stopped, or another
    /// generation's container runs.
    StaleGeneration {
        /// The generation the launcher holds as current.
        current: u64,
    },
    /// A container of the workspace exists (for `reset_home`, `destroy`).
    Busy,
    /// No such workspace, container or volume.
    NotFound,
    /// The request is malformed (for example an invalid workspace id).
    InvalidRequest,
    /// The container runtime failed.
    RuntimeFailed,
    /// The requested image digest is not the launcher's configured one.
    ImageMismatch,
}

#[cfg(test)]
mod tests;
