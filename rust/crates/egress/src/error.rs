//! Errors of starting a proxy. A running proxy answers refusals on the
//! connection and records denials; it has no error path to its owner.

use std::io;

/// Why a proxy could not start.
#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    /// A bound in [`crate::EgressConfig`] is unusable.
    #[error("invalid egress config: {0}")]
    InvalidConfig(&'static str),
    /// The socket path exists already, or binding failed otherwise.
    #[error("cannot bind the egress socket: {0}")]
    Bind(#[source] io::Error),
    /// The socket mode could not be set; the socket file was removed.
    #[error("cannot set the egress socket mode: {0}")]
    Permissions(#[source] io::Error),
}
