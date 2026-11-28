//! Error types: configuration, startup and shutdown (public), and the
//! service's refusals, which become a [`LauncherError`] on the wire.

use std::io;

use agentkit_sandbox_proto::launcher::{LauncherError, LauncherErrorCode};

/// Longest message the launcher sends or logs for one failure.
pub(crate) const MESSAGE_MAX_CHARS: usize = 200;

/// Why a configuration was refused.
#[derive(Debug, thiserror::Error)]
pub enum VmConfigError {
    /// A value is out of range or inconsistent.
    #[error("invalid microVM launcher config: {0}")]
    Invalid(&'static str),
}

/// Why the launcher could not start serving.
#[derive(Debug, thiserror::Error)]
pub enum VmStartupError {
    /// The configuration is not valid.
    #[error(transparent)]
    Config(#[from] VmConfigError),
    /// The data root or socket root cannot be created, is a link, belongs
    /// to another user or grants group or others access.
    #[error("directory refused: {0}")]
    Directory(String),
    /// The helper binary is missing or not a regular file.
    #[error("helper refused: {0}")]
    Helper(String),
    /// The kernel or root disk cannot be read or does not match its
    /// configured SHA-256.
    #[error("image refused: {0}")]
    Image(String),
    /// The state cannot be read, or another launcher holds it.
    #[error("state refused: {0}")]
    State(String),
    /// A helper of an earlier run that is still ours could not be killed
    /// and proven gone; the launcher does not start beside it.
    #[error("recovery failed: {0}")]
    Recovery(String),
    /// The launcher's socket cannot be bound.
    #[error("cannot listen: {0}")]
    Listen(#[source] io::Error),
}

/// What [`crate::VmLauncher::shutdown`] could not prove.
#[derive(Debug, thiserror::Error)]
pub enum VmShutdownError {
    /// These workspaces' helpers were not proven stopped.
    #[error("helpers not proven stopped: {}", .0.join(", "))]
    Unproven(Vec<String>),
}

/// A refused or failed request, before it becomes a wire error.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServiceError {
    /// Malformed request: bad id, generation, tier.
    #[error("{0}")]
    Invalid(String),
    /// The requested generation is at or below one the launcher holds.
    #[error("generation {requested} is stale; current is {current}")]
    Stale {
        /// Generation in the request.
        requested: u64,
        /// Highest generation the launcher knows for the workspace.
        current: u64,
    },
    /// A machine of the workspace exists that blocks the request.
    #[error("{0}")]
    Busy(String),
    /// The image digest is not the launcher's, or an image file changed.
    #[error("{0}")]
    ImageMismatch(String),
    /// `max_machines` helpers run already; nothing was recorded.
    #[error("{0}")]
    AtCapacity(String),
    /// A process still has the workspace's disks open (a VM that outlived
    /// its helper); nothing was recorded.
    #[error("{0}")]
    DisksHeld(String),
    /// The launcher is shutting down and starts nothing more.
    #[error("the launcher is shutting down")]
    ShuttingDown,
    /// The helper failed to start or to connect.
    #[error("{0}")]
    Helper(String),
    /// The launcher's own files (state, disks, socket directories) failed.
    #[error("{0}")]
    Local(String),
}

impl ServiceError {
    /// The wire form, message bounded.
    pub(crate) fn to_wire(&self) -> LauncherError {
        let code = match self {
            ServiceError::Invalid(_) => LauncherErrorCode::InvalidRequest,
            ServiceError::Stale { current, .. } => {
                LauncherErrorCode::StaleGeneration { current: *current }
            }
            ServiceError::Busy(_) => LauncherErrorCode::Busy,
            ServiceError::ImageMismatch(_) => LauncherErrorCode::ImageMismatch,
            // Capacity is transient, as in the container launcher: `busy`
            // at an ensure would make the adapter answer the generation
            // stale and the service burn generations over a full host.
            ServiceError::AtCapacity(_)
            | ServiceError::DisksHeld(_)
            | ServiceError::ShuttingDown
            | ServiceError::Helper(_)
            | ServiceError::Local(_) => LauncherErrorCode::RuntimeFailed,
        };
        LauncherError {
            code,
            message: bounded(&self.to_string()),
        }
    }
}

/// `text` cut to [`MESSAGE_MAX_CHARS`] characters, control characters
/// replaced, so a helper's or the system's text cannot flood a reply.
pub(crate) fn bounded(text: &str) -> String {
    text.chars()
        .take(MESSAGE_MAX_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
