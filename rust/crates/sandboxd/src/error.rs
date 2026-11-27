//! Public errors: why the supervisor did not start or did not greet its
//! host. Every discriminant is the outcome class; strings are context.

use std::io;
use std::path::PathBuf;

use agentkit_sandbox_proto::FrameError;

/// The launcher's environment does not describe a usable machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A required variable is not set.
    #[error("{0} is not set")]
    Missing(&'static str),
    /// A variable is set to a value the supervisor cannot use.
    #[error("{name} is invalid: {reason}")]
    Invalid {
        /// The variable.
        name: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Two variables name different transports for one link.
    #[error("{name} and {with} are both set")]
    Conflict {
        /// The vsock port variable.
        name: &'static str,
        /// The unix socket variable of the same link.
        with: &'static str,
    },
    /// A variable asks for a transport this platform lacks (vsock off
    /// Linux).
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
}

/// The first exchange on the control channel failed; the supervisor
/// closes the channel and exits without serving anything.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// No `hello` arrived within the handshake window.
    #[error("no hello arrived in time")]
    Timeout,
    /// The host closed the channel before `hello`.
    #[error("the host closed the channel before hello")]
    Closed,
    /// The first frame is not a `hello` request.
    #[error("the first message is not hello")]
    NotHello,
    /// The host speaks another protocol version.
    #[error("the host speaks protocol {got}, the guest {expected}")]
    ProtocolMismatch {
        /// The host's version.
        got: u32,
        /// The guest's version.
        expected: u32,
    },
    /// The host believes the machine serves another generation.
    #[error("the host names generation {got}, the guest serves {expected}")]
    GenerationMismatch {
        /// The host's generation.
        got: u64,
        /// The guest's generation.
        expected: u64,
    },
    /// The channel broke the frame protocol or failed.
    #[error("control channel failed: {0}")]
    Channel(#[source] FrameError),
}

/// Why [`crate::run`] or [`crate::run_with`] returned before serving.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The control socket did not accept a connection within the dial
    /// window.
    #[error("control socket {} did not accept within {secs} s: {source}", path.display())]
    Dial {
        /// The socket path.
        path: PathBuf,
        /// The dial window in seconds.
        secs: u64,
        /// The last connect error.
        #[source]
        source: io::Error,
    },
    /// The control vsock port could not be listened on.
    #[error("cannot listen on vsock port {port}: {source}")]
    Listen {
        /// The vsock port.
        port: u32,
        /// Why the bind or listen failed.
        #[source]
        source: io::Error,
    },
    /// No control connection arrived on the vsock port within the accept
    /// window.
    #[error("no control connection on vsock port {port} within {secs} s: {source}")]
    Accept {
        /// The vsock port.
        port: u32,
        /// The accept window in seconds.
        secs: u64,
        /// The last accept error, or a timeout when none failed.
        #[source]
        source: io::Error,
    },
    /// The handshake failed.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
}
