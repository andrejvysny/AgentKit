//! The control channel: dial, handshake, then serve requests concurrently
//! until `shutdown` or until the channel is lost.
//!
//! Every request runs in its own task, so `output`, `read` and `list`
//! answer while a command runs; replies carry their request's id and one
//! writer task serializes them. A `write`'s data frames are routed to its
//! task by request id. A frame that breaks the protocol (an undecodable
//! frame, data for no open stream, a second stream under one id) closes
//! the channel. Whichever way the channel ends, every process the
//! operations started is ended first (see [`crate::reaper`]), then the
//! filesystems are synced (not in [`ProcessMode::Embedded`]); `shutdown`
//! answers `bye` only after both.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentkit_sandbox_proto::guest::{GuestErrorCode, PathRoot, Reply, Request, RequestEnvelope};
use agentkit_sandbox_proto::{DataFrame, Envelope, Frame, PROTOCOL_VERSION, parse_control};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc};

use crate::config::{Config, ProcessMode};
use crate::conn::{FrameReader, Replier, spawn_writer};
use crate::error::{HandshakeError, StartupError};
use crate::optable::{OpTable, is_terminal};
use crate::reaper::Reaper;
use crate::shim::{SHIM_MAX_CONNECTIONS, bind, run_shim};
use crate::{envfile, exec, files, listing, output, usage, write};

/// How long the supervisor retries its dial of the control socket.
const DIAL_WINDOW: Duration = Duration::from_secs(30);
const DIAL_RETRY: Duration = Duration::from_millis(200);
/// How long the host has to send `hello`.
const HANDSHAKE_WINDOW: Duration = Duration::from_secs(30);
/// Data frames buffered per open write stream.
const STREAM_DEPTH: usize = 4;
/// How long operations get to record their receipts after the sweep.
const SETTLE_LIMIT: Duration = Duration::from_secs(5);

/// How serving ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The host sent `shutdown`; `bye` was answered.
    Shutdown,
    /// The control channel closed or broke.
    ChannelLost,
}

/// State shared by every request of one channel.
pub(crate) struct Server {
    pub(crate) config: Config,
    /// The command environment from the image's environment file.
    pub(crate) env: Vec<(String, String)>,
    pub(crate) ops: OpTable,
    pub(crate) reaper: Arc<Reaper>,
    pub(crate) out: Replier,
    /// Held while a command's log is set up, so a repair of the log
    /// directory never races another command's setup.
    pub(crate) log_setup: Mutex<()>,
    temp_seq: AtomicU64,
}

impl Server {
    /// The directory a path root names.
    pub(crate) fn root(&self, root: PathRoot) -> &Path {
        match root {
            PathRoot::Workspace => &self.config.workspace_root,
            PathRoot::Home => &self.config.home_root,
        }
    }

    /// A number for the next temp file name.
    pub(crate) fn next_temp(&self) -> u64 {
        self.temp_seq.fetch_add(1, Ordering::Relaxed)
    }
}

/// Start the egress shim, dial the control socket, handshake and serve.
pub async fn run(config: Config) -> Result<Exit, StartupError> {
    let shim = match bind(config.shim_addr).await {
        Ok(listener) => Some(tokio::spawn(run_shim(
            listener,
            config.egress_socket.clone(),
            SHIM_MAX_CONNECTIONS,
        ))),
        Err(error) => {
            eprintln!("agentkit-sandboxd: egress shim not started: {error}");
            None
        }
    };
    let stream = dial(&config.control_socket).await;
    let outcome = match stream {
        Ok(stream) => serve(config, stream).await.map_err(StartupError::from),
        Err(error) => Err(error),
    };
    if let Some(shim) = shim {
        shim.abort();
    }
    outcome
}

async fn dial(path: &Path) -> Result<UnixStream, StartupError> {
    let deadline = Instant::now() + DIAL_WINDOW;
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => {
                eprintln!("agentkit-sandboxd: connected to the control socket");
                return Ok(stream);
            }
            Err(error) if Instant::now() >= deadline => {
                return Err(StartupError::Dial {
                    path: path.to_path_buf(),
                    secs: DIAL_WINDOW.as_secs(),
                    source: error,
                });
            }
            Err(_) => tokio::time::sleep(DIAL_RETRY).await,
        }
    }
}

/// Handshake on `stream`, then serve it until `shutdown` or until it is
/// lost. Every process the operations started is gone when this returns.
pub async fn serve<S>(config: Config, stream: S) -> Result<Exit, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read, write) = tokio::io::split(stream);
    let mut reader = FrameReader::new(read);
    let broken = Arc::new(Notify::new());
    let (out, _writer) = spawn_writer(write, broken.clone());
    if let Err(error) = handshake(&config, &mut reader, &out).await {
        eprintln!("agentkit-sandboxd: handshake failed: {error}");
        out.flush().await;
        return Err(error);
    }
    eprintln!(
        "agentkit-sandboxd: serving generation {}",
        config.generation
    );
    let reaper = Arc::new(Reaper::new(config.mode));
    let background = (config.mode != ProcessMode::Embedded)
        .then(|| tokio::spawn(reaper.clone().reap_in_background()));
    let server = Arc::new(Server {
        env: envfile::load(&config.env_file),
        config,
        ops: OpTable::default(),
        reaper,
        out,
        log_setup: Mutex::new(()),
        temp_seq: AtomicU64::new(0),
    });
    let ending = serve_loop(&server, &mut reader, &broken).await;
    end_everything(&server).await;
    if let Some(background) = background {
        background.abort();
    }
    match ending {
        Ending::Shutdown(id) => {
            server.out.reply(id, Reply::Bye).await;
            server.out.flush().await;
            eprintln!("agentkit-sandboxd: shut down");
            Ok(Exit::Shutdown)
        }
        Ending::Lost => {
            eprintln!("agentkit-sandboxd: control channel lost");
            Ok(Exit::ChannelLost)
        }
    }
}

async fn handshake<R: AsyncRead + Unpin>(
    config: &Config,
    reader: &mut FrameReader<R>,
    out: &Replier,
) -> Result<(), HandshakeError> {
    let (id, protocol, generation) = read_hello(reader).await?;
    if protocol != PROTOCOL_VERSION {
        let message = "unsupported protocol version";
        out.error(id, GuestErrorCode::ProtocolMismatch, message)
            .await;
        return Err(HandshakeError::ProtocolMismatch {
            got: protocol,
            expected: PROTOCOL_VERSION,
        });
    }
    if generation != config.generation {
        out.error(id, GuestErrorCode::GenerationMismatch, "another generation")
            .await;
        return Err(HandshakeError::GenerationMismatch {
            got: generation,
            expected: config.generation,
        });
    }
    let reply = Reply::Hello {
        protocol: PROTOCOL_VERSION,
        generation,
        image_digest: config.image_digest.clone(),
        channel_token: config.channel_token.clone(),
    };
    out.reply(id, reply).await;
    Ok(())
}

/// The first frame, which must be `hello`: its id, protocol and
/// generation.
async fn read_hello<R: AsyncRead + Unpin>(
    reader: &mut FrameReader<R>,
) -> Result<(u64, u32, u64), HandshakeError> {
    let frame = tokio::time::timeout(HANDSHAKE_WINDOW, reader.next())
        .await
        .map_err(|_| HandshakeError::Timeout)?
        .map_err(HandshakeError::Channel)?;
    let json = match frame {
        None => return Err(HandshakeError::Closed),
        Some(Frame::Data(_)) => return Err(HandshakeError::NotHello),
        Some(Frame::Control(json)) => json,
    };
    match parse_control::<RequestEnvelope>(&json) {
        Ok(Envelope {
            id,
            body:
                Request::Hello {
                    protocol,
                    generation,
                },
        }) => Ok((id, protocol, generation)),
        _ => Err(HandshakeError::NotHello),
    }
}

enum Ending {
    Shutdown(u64),
    Lost,
}

enum Flow {
    Continue,
    Shutdown(u64),
    Violation,
}

async fn serve_loop<R: AsyncRead + Unpin>(
    server: &Arc<Server>,
    reader: &mut FrameReader<R>,
    broken: &Notify,
) -> Ending {
    let mut routes = Routes::default();
    loop {
        let frame = tokio::select! {
            frame = reader.next() => frame,
            () = broken.notified() => return Ending::Lost,
        };
        let flow = match frame {
            Ok(Some(Frame::Control(json))) => on_control(server, &mut routes, &json).await,
            Ok(Some(Frame::Data(data))) => routes.forward(data).await,
            Ok(None) | Err(_) => return Ending::Lost,
        };
        match flow {
            Flow::Continue => {}
            Flow::Shutdown(id) => return Ending::Shutdown(id),
            Flow::Violation => return Ending::Lost,
        }
    }
}

async fn on_control(server: &Arc<Server>, routes: &mut Routes, json: &str) -> Flow {
    let Ok(Envelope { id, body }) = parse_control::<RequestEnvelope>(json) else {
        return malformed(server, routes, json).await;
    };
    let server = server.clone();
    match body {
        Request::Shutdown => return Flow::Shutdown(id),
        Request::Write(req) => {
            let Some(frames) = routes.open(id) else {
                return Flow::Violation;
            };
            tokio::spawn(write::handle_write(server, id, req, frames));
        }
        other => {
            tokio::spawn(handle(server, id, other));
        }
    }
    Flow::Continue
}

/// A control frame that is JSON but not a request: answered
/// `invalid_request` when it carries an id (a `write`'s stream is then
/// read and dropped); without an id the channel closes.
async fn malformed(server: &Server, routes: &mut Routes, json: &str) -> Flow {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Flow::Violation;
    };
    let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) else {
        return Flow::Violation;
    };
    let is_write = value
        .pointer("/body/type")
        .and_then(serde_json::Value::as_str)
        == Some("write");
    if is_write && !routes.discard(id) {
        return Flow::Violation;
    }
    server
        .out
        .error(id, GuestErrorCode::InvalidRequest, "not a valid request")
        .await;
    Flow::Continue
}

async fn handle(server: Arc<Server>, id: u64, request: Request) {
    match request {
        Request::Exec(req) => exec::handle_exec(server, id, req).await,
        Request::Status { op } => {
            let reply = server
                .ops
                .get(&op)
                .map_or(Reply::Unknown, |entry| Reply::State {
                    state: entry.state(),
                });
            server.out.reply(id, reply).await;
        }
        Request::Output {
            op,
            after_seq,
            max_bytes,
        } => output::handle_output(server, id, op, after_seq, max_bytes).await,
        Request::Cancel { op } => cancel(&server, id, &op).await,
        Request::Read(req) => files::handle_read(server, id, req).await,
        Request::List(req) => listing::handle_list(server, id, req).await,
        Request::Usage => usage::handle_usage(server, id).await,
        Request::Hello { .. } => {
            server
                .out
                .error(id, GuestErrorCode::InvalidRequest, "already greeted")
                .await;
        }
        // Routed by `on_control`.
        Request::Write(_) | Request::Shutdown => {}
    }
}

/// `cancel`: asks a running command to end and answers the state now; the
/// receipt follows through `status` or `output`.
async fn cancel(server: &Server, id: u64, op: &str) {
    let Some(entry) = server.ops.get(op) else {
        server.out.reply(id, Reply::Unknown).await;
        return;
    };
    if let Some(exec) = entry.exec()
        && !is_terminal(entry.state())
    {
        exec.cancel.notify_one();
    }
    let reply = Reply::State {
        state: entry.state(),
    };
    server.out.reply(id, reply).await;
}

/// End every process, let the operations record their receipts, sync.
async fn end_everything(server: &Server) {
    server.reaper.end_all().await;
    let deadline = Instant::now() + SETTLE_LIMIT;
    while server.ops.exec_open() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if server.config.mode != ProcessMode::Embedded {
        let _ = tokio::task::spawn_blocking(agentkit_sys::sync_filesystems).await;
    }
}

/// Open data streams by request id. A stream's task may stop reading
/// early; its remaining frames are then dropped here up to the terminator.
#[derive(Default)]
struct Routes {
    open: HashMap<u64, Option<mpsc::Sender<DataFrame>>>,
}

impl Routes {
    /// Open the stream of request `id`; `None` when one is open already.
    fn open(&mut self, id: u64) -> Option<mpsc::Receiver<DataFrame>> {
        if self.open.contains_key(&id) {
            return None;
        }
        let (tx, rx) = mpsc::channel(STREAM_DEPTH);
        self.open.insert(id, Some(tx));
        Some(rx)
    }

    /// Drop the stream of request `id`; false when one is open already.
    fn discard(&mut self, id: u64) -> bool {
        if self.open.contains_key(&id) {
            return false;
        }
        self.open.insert(id, None);
        true
    }

    async fn forward(&mut self, frame: DataFrame) -> Flow {
        let id = frame.id;
        let last = frame.bytes.is_empty();
        let Some(slot) = self.open.get_mut(&id) else {
            return Flow::Violation;
        };
        if let Some(tx) = slot
            && tx.send(frame).await.is_err()
        {
            *slot = None;
        }
        if last {
            self.open.remove(&id);
        }
        Flow::Continue
    }
}
