//! Error types: configuration and startup (public), and the service's
//! refusals, which become a [`LauncherError`] on the wire.

use std::io;

use agentkit_sandbox_proto::launcher::{LauncherError, LauncherErrorCode};

use crate::runtime::RuntimeError;

/// Longest message the launcher sends or logs for one failure. Runtime
/// error bodies are cut to it and never passed on whole.
pub(crate) const MESSAGE_MAX_CHARS: usize = 200;

/// Why a configuration file was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file cannot be read.
    #[error("cannot read the configuration: {0}")]
    Read(#[source] io::Error),
    /// The file is not valid JSON of the expected shape.
    #[error("configuration is not valid: {0}")]
    Parse(#[source] serde_json::Error),
    /// A value is out of range or inconsistent.
    #[error("configuration is not valid: {0}")]
    Invalid(String),
}

/// Why the launcher could not start serving.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The socket host directory is missing, reached through a link, open
    /// to others, or below a directory another user could change.
    #[error("socket host directory refused: {0}")]
    SocketDir(String),
    /// The state directory or file cannot be read, or the directory or one
    /// above it could be changed by another user; the launcher never
    /// starts without its stopped-through records.
    #[error("state refused: {0}")]
    State(String),
    /// The listen socket cannot be bound or its mode set, or its directory
    /// could be changed by another user.
    #[error("cannot listen: {0}")]
    Listen(#[source] io::Error),
    /// The accept loop failed for good.
    #[error("listener failed: {0}")]
    Serve(#[source] io::Error),
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
    /// A container of the workspace exists that blocks the request.
    #[error("{0}")]
    Busy(String),
    /// The image digest is not the configured one.
    #[error("{0}")]
    ImageMismatch(String),
    /// The launcher holds `max_containers` already; nothing was created.
    #[error("{0}")]
    AtCapacity(String),
    /// The container runtime failed.
    #[error("{0}")]
    Runtime(#[from] RuntimeError),
    /// The launcher's own files (state, socket directories) failed.
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
            // Capacity is transient: `runtime_failed` makes the adapter
            // answer `Unavailable`. `busy` at an ensure would make it answer
            // the generation stale, and the service would burn generations
            // and fence the workspace over a full host.
            ServiceError::Runtime(_) | ServiceError::Local(_) | ServiceError::AtCapacity(_) => {
                LauncherErrorCode::RuntimeFailed
            }
        };
        LauncherError {
            code,
            message: bounded(&self.to_string()),
        }
    }
}

/// `text` cut to [`MESSAGE_MAX_CHARS`] characters, control characters
/// replaced, so a runtime's error body cannot flood a reply or a log.
pub(crate) fn bounded(text: &str) -> String {
    text.chars()
        .take(MESSAGE_MAX_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
