//! The launcher's unix-socket server: length-prefixed control frames of
//! `agentkit-sandbox-proto::launcher`, one request per frame, each reply
//! carrying its request's id.
//!
//! - Access: the socket's mode and group (configuration), and the peer's
//!   uid must be the server's or the launcher's own.
//! - At most `max_connections` connections are served; further ones wait
//!   in the listen backlog. Each connection runs up to [`IN_FLIGHT_MAX`]
//!   requests at once (a slow `stop` of one workspace does not hold the
//!   others), and one writer task owns the write half.
//! - A request runs to the end even when its client disconnects, so a
//!   half-done runtime call is never abandoned; its reply is then dropped.
//! - A data frame, a frame error or an envelope without a readable id
//!   closes the connection; a readable id with a bad body is answered
//!   `invalid_request`.

use std::sync::Arc;
use std::time::Duration;

use agentkit_sandbox_proto::launcher::{LauncherError, LauncherErrorCode, Reply, Request};
use agentkit_sandbox_proto::{Envelope, Frame, FrameDecoder, FrameError, encode_control};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, mpsc};

use crate::error::bounded;
use crate::service::{Service, log};

/// Requests of one connection running at once.
pub(crate) const IN_FLIGHT_MAX: usize = 8;
/// Replies queued for the writer.
const QUEUE_DEPTH: usize = 16;
/// Bytes read from the socket per call.
const READ_CHUNK: usize = 64 * 1024;
/// Pause after a failed accept (descriptor exhaustion), so the loop does
/// not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Who may talk to the launcher.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Peers {
    pub server_uid: u32,
    pub own_uid: u32,
}

impl Peers {
    fn allows(&self, stream: &UnixStream) -> bool {
        match stream.peer_cred() {
            Ok(cred) => cred.uid() == self.server_uid || cred.uid() == self.own_uid,
            Err(_) => false,
        }
    }
}

/// Accept connections until the listener fails.
pub(crate) async fn serve(
    listener: UnixListener,
    service: Arc<Service>,
    peers: Peers,
    max_connections: usize,
) -> std::io::Result<()> {
    let slots = Arc::new(Semaphore::new(max_connections));
    loop {
        let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
            return Ok(());
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                log(&format!("accept failed: {error}"));
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        if !peers.allows(&stream) {
            log("refused a connection from another user");
            continue;
        }
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            serve_connection(stream, service).await;
            drop(permit);
        });
    }
}

/// Serve one connection until the peer closes it or breaks the protocol,
/// then wait for its requests to finish.
pub(crate) async fn serve_connection(stream: UnixStream, service: Arc<Service>) {
    let (read, write) = stream.into_split();
    let (tx, rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);
    let writer = tokio::spawn(write_loop(write, rx));
    let in_flight = Arc::new(Semaphore::new(IN_FLIGHT_MAX));
    let mut reader = FrameReader::new(read);
    while let Ok(Some(Frame::Control(json))) = reader.next().await {
        let Some((id, request)) = parse_request(&json) else {
            break;
        };
        let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
            break;
        };
        let service = Arc::clone(&service);
        let tx = tx.clone();
        tokio::spawn(async move {
            let reply = match request {
                Ok(request) => service.handle(request).await,
                Err(message) => invalid_request(message),
            };
            let _ = tx.send(encode_reply(id, &reply)).await;
            drop(permit);
        });
    }
    let _ = in_flight.acquire_many(IN_FLIGHT_MAX as u32).await;
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

#[cfg(test)]
mod tests;
