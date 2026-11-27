//! The microVM guest's links to its host (ADR 0040 §4): the control
//! channel accepted once on a vsock port the supervisor listens on, and
//! the egress shim's connections to the host's vsock port. Accepting once
//! is generic over the listener, so its tests run over a unix listener
//! where vsock does not exist; only the vsock bindings are Linux-only.

use std::future::Future;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;

use crate::error::StartupError;

const ACCEPT_RETRY: Duration = Duration::from_millis(200);

/// A bound listener the control channel is accepted from.
pub(crate) trait ControlListener: Send + Sync {
    /// The accepted connection.
    type Stream: AsyncRead + AsyncWrite + Send + 'static;

    /// The next connection.
    fn accept_stream(&self) -> impl Future<Output = io::Result<Self::Stream>> + Send;
}

/// Accept exactly one connection on `listener` within `window` and close
/// the listener before the session starts: the host connects once per
/// machine lifetime, and no later connection (a guest process exists only
/// once the session runs commands) is ever answered. Accept errors are
/// retried until the window ends.
pub(crate) async fn accept_once<L: ControlListener>(
    listener: L,
    port: u32,
    window: Duration,
) -> Result<L::Stream, StartupError> {
    let deadline = Instant::now() + window;
    let mut last = io::Error::from(io::ErrorKind::TimedOut);
    while let Ok(accepted) = tokio::time::timeout_at(deadline, listener.accept_stream()).await {
        match accepted {
            Ok(stream) => {
                drop(listener);
                eprintln!(
                    "agentkit-sandboxd: accepted the control connection on vsock port {port}"
                );
                return Ok(stream);
            }
            Err(error) => last = error,
        }
        tokio::time::sleep(ACCEPT_RETRY).await;
    }
    Err(StartupError::Accept {
        port,
        secs: window.as_secs(),
        source: last,
    })
}

#[cfg(target_os = "linux")]
pub(crate) use vsock::{connect_host, listen};

#[cfg(target_os = "linux")]
mod vsock {
    use std::io;

    use tokio_vsock::{VMADDR_CID_ANY, VMADDR_CID_HOST, VsockAddr, VsockListener, VsockStream};

    use super::ControlListener;
    use crate::error::StartupError;

    /// Listen on `port` for any CID. Sockets are close-on-exec from
    /// creation (`SOCK_CLOEXEC`, `accept4`), so none reaches a command.
    pub(crate) fn listen(port: u32) -> Result<VsockListener, StartupError> {
        VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))
            .map_err(|source| StartupError::Listen { port, source })
    }

    impl ControlListener for VsockListener {
        type Stream = VsockStream;

        async fn accept_stream(&self) -> io::Result<VsockStream> {
            self.accept().await.map(|(stream, _peer)| stream)
        }
    }

    /// Connect to the host's `port`.
    pub(crate) async fn connect_host(port: u32) -> io::Result<VsockStream> {
        VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, port)).await
    }
}

#[cfg(test)]
mod tests;
