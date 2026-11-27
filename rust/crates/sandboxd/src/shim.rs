//! The egress shim: a TCP listener on `127.0.0.1:3128` inside the
//! container that pipes each connection to the host's egress proxy over
//! the egress unix socket, or in a microVM guest over vsock to the host.
//! The container has no network; the image points `HTTP_PROXY` and
//! friends here. The shim reads nothing: the proxy on the host decides
//! what may leave. When the upstream is absent or refuses, the client's
//! connection just closes. At most [`SHIM_MAX_CONNECTIONS`] are piped at
//! once; more are closed at accept.

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::sync::Semaphore;

/// Where the shim pipes each connection.
#[derive(Debug, Clone)]
pub(crate) enum Upstream {
    /// The host's egress proxy socket (container).
    Socket(PathBuf),
    /// The host (CID 2) on this vsock port (microVM guest).
    #[cfg(target_os = "linux")]
    Vsock(u32),
}

/// Most connections piped at once.
pub(crate) const SHIM_MAX_CONNECTIONS: usize = 64;
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Bind the shim's listener.
pub(crate) async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Accept and pipe connections until the task is dropped.
pub(crate) async fn run_shim(listener: TcpListener, egress: Upstream, max: usize) {
    let permits = Arc::new(Semaphore::new(max));
    let egress = Arc::new(egress);
    loop {
        let client = match listener.accept().await {
            Ok((client, _)) => client,
            Err(_) => {
                // Out of descriptors or similar: back off, keep serving.
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            drop(client);
            continue;
        };
        let egress = egress.clone();
        tokio::spawn(async move {
            pipe_one(client, &egress).await;
            drop(permit);
        });
    }
}

async fn pipe_one(client: TcpStream, egress: &Upstream) {
    match egress {
        Upstream::Socket(path) => pipe(client, UnixStream::connect(path).await).await,
        #[cfg(target_os = "linux")]
        Upstream::Vsock(port) => pipe(client, crate::transport::connect_host(*port).await).await,
    }
}

async fn pipe<S>(mut client: TcpStream, upstream: io::Result<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(mut upstream) = upstream else {
        return;
    };
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

#[cfg(test)]
mod tests;
