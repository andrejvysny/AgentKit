//! The launcher's unix-socket server, as in `agentkit-sandbox-launcher`:
//! length-prefixed control frames of `agentkit-sandbox-proto::launcher`,
//! one request per frame, each reply carrying its request's id.
//!
//! - Access: the socket is mode 0600 in the 0700 socket root, and the
//!   peer's uid must be this process's own.
//! - At most `max_connections` connections are served; further ones wait
//!   in the listen backlog. Each connection runs up to [`IN_FLIGHT_MAX`]
//!   requests at once, and one writer task owns the write half.
//! - A request runs to the end even when its client disconnects; its reply
//!   is then dropped. Every task belongs to the server's task set, so
//!   stopping the server ends them all.
//! - A data frame, a frame error or an envelope without a readable id
//!   closes the connection; a readable id with a bad body is answered
//!   `invalid_request`.

use std::fs::Permissions;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::{LauncherError, LauncherErrorCode, Reply, Request};
use agentkit_sandbox_proto::{Envelope, Frame, FrameDecoder, FrameError, encode_control};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::error::bounded;
use crate::service::{Service, log};

/// Requests of one connection running at once.
const IN_FLIGHT_MAX: usize = 8;
/// Replies queued for the writer.
const QUEUE_DEPTH: usize = 16;
/// Bytes read from the socket per call.
const READ_CHUNK: usize = 64 * 1024;
/// Pause after a failed accept (descriptor exhaustion).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Mode of the listen socket.
const SOCKET_MODE: u32 = 0o600;

/// Replace a stale socket file at `path` (never anything else), bind, and
/// set mode 0600. The socket root is private, so nobody can swap the
/// socket between the bind and the mode change.
pub(crate) fn bind(path: &Path) -> io::Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the listen socket path exists and is not a socket",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE))?;
    Ok(listener)
}

/// Accept connections from `own_uid` until the task is aborted.
pub(crate) async fn serve(
    listener: UnixListener,
    service: Arc<Service>,
    own_uid: u32,
    max_connections: usize,
) {
    let slots = Arc::new(Semaphore::new(max_connections));
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                log(&format!("accept failed: {error}"));
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        if !stream.peer_cred().is_ok_and(|cred| cred.uid() == own_uid) {
            log("refused a connection from another user");
            continue;
        }
        let service = Arc::clone(&service);
        connections.spawn(async move {
            serve_connection(stream, service).await;
            drop(permit);
        });
    }
}

/// Serve one connection until the peer closes it or breaks the protocol,
/// then wait for its requests to finish.
async fn serve_connection(stream: UnixStream, service: Arc<Service>) {
    let (read, write) = stream.into_split();
    let (tx, rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);
    let writer = tokio::spawn(write_loop(write, rx));
    let in_flight = Arc::new(Semaphore::new(IN_FLIGHT_MAX));
    let mut requests = JoinSet::new();
    let mut reader = FrameReader::new(read);
    while let Ok(Some(Frame::Control(json))) = reader.next().await {
        let Some((id, request)) = parse_request(&json) else {
            break;
        };
        let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
            break;
        };
        let (service, tx) = (Arc::clone(&service), tx.clone());
        requests.spawn(async move {
            let reply = match request {
                Ok(request) => service.handle(request).await,
                Err(message) => invalid_request(message),
            };
            let _ = tx.send(encode_reply(id, &reply)).await;
            drop(permit);
        });
    }
    while requests.join_next().await.is_some() {}
    drop(tx);
    let _ = writer.await;
}

/// The envelope's id and its request, or the reason the body is not one.
/// `None` when not even the id can be read.
fn parse_request(json: &str) -> Option<(u64, Result<Request, String>)> {
    let envelope: Envelope<Value> = serde_json::from_str(json).ok()?;
    let request = serde_json::from_value::<Request>(envelope.body)
        .map_err(|error| bounded(&format!("not a launcher request: {error}")));
    Some((envelope.id, request))
}

fn invalid_request(message: String) -> Reply {
    Reply::Error(LauncherError {
        code: LauncherErrorCode::InvalidRequest,
        message,
    })
}

fn encode_reply(id: u64, reply: &Reply) -> Vec<u8> {
    encode_control(&Envelope::new(id, reply)).unwrap_or_else(|_| {
        let fallback = Reply::Error(LauncherError {
            code: LauncherErrorCode::RuntimeFailed,
            message: "reply too large to encode".into(),
        });
        encode_control(&Envelope::new(id, fallback)).unwrap_or_default()
    })
}

async fn write_loop(mut write: OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if write.write_all(&frame).await.is_err() || write.flush().await.is_err() {
            // Keep draining so senders never block on a dead connection.
            while rx.recv().await.is_some() {}
            return;
        }
    }
    let _ = write.shutdown().await;
}

/// Decodes frames from the read half.
struct FrameReader {
    inner: OwnedReadHalf,
    decoder: FrameDecoder,
    buf: Vec<u8>,
}

impl FrameReader {
    fn new(inner: OwnedReadHalf) -> Self {
        Self {
            inner,
            decoder: FrameDecoder::new(),
            buf: vec![0; READ_CHUNK],
        }
    }

    /// The next frame; `None` when the peer closed between frames.
    async fn next(&mut self) -> Result<Option<Frame>, FrameError> {
        loop {
            if let Some(frame) = self.decoder.next_frame()? {
                return Ok(Some(frame));
            }
            let read = self.inner.read(&mut self.buf).await?;
            if read == 0 {
                if self.decoder.buffered() > 0 {
                    return Err(FrameError::Truncated);
                }
                return Ok(None);
            }
            self.decoder.push(&self.buf[..read]);
        }
    }
}
