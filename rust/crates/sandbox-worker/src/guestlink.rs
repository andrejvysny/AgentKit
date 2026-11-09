//! Worker→guest connection pool and link abstraction.
//!
//! The guest **listens** on a vsock port (`--guest-vsock-port`, default 52);
//! the worker connects after VM launch. For `--backend fake` the "guest" is
//! an in-process simulator speaking the same `WorkerToGuest`/`GuestToWorker`
//! catalogs over a duplex byte stream with the real boot-nonce handshake.
//!
//! Trust: guest messages are always untrusted input (S17). The link layer
//! only frames/transports; the service validates digests, nonces, and never
//! treats guest-claimed state as authority.

use crate::error::{WorkerError, WorkerResult};
use agentkit_sandbox_contracts::api::ExecEventKind;
use agentkit_sandbox_contracts::api::messages::GuestOpOutcome;
use agentkit_sandbox_contracts::api::messages::{GuestToWorker, WorkerToGuest};
use agentkit_sandbox_contracts::ids::{ExecutionId, InstanceGeneration, SandboxId};
use agentkit_sandbox_contracts::limits;
use agentkit_sandbox_contracts::protocol::{FrameDecoder, FrameHeader, WireFrame, encode_frame};
use async_trait::async_trait;
use base64::Engine as _;
use std::path::PathBuf;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};

/// Default vsock port the guest agent listens on.
pub const GUEST_VSOCK_PORT: u32 = 52;

/// How to reach one sandbox generation's guest agent. Paths are derived by
/// the worker/backend under its private storage root — never accepted from
/// the wire (S20).
#[derive(Debug)]
pub enum GuestEndpoint {
    /// Real transport: `api_uds` is the jailer vsock UDS (FC CONNECT
    /// protocol); when empty, plain AF_VSOCK to `cid:port` is used (Linux).
    Vsock {
        api_uds: PathBuf,
        port: u32,
        cid: u32,
    },
    /// In-process fake transport. Constructible only by the fake backend so
    /// the firecracker path can never be fed a fake link.
    Fake { stream: Mutex<Option<DuplexStream>> },
    /// Placeholder for registry records rebuilt by journal replay before a
    /// reconcile replaces them with a live endpoint.
    Unresolved,
}

impl GuestEndpoint {
    /// Real vsock endpoint (worker-derived paths only).
    #[must_use]
    pub fn vsock(api_uds: PathBuf, port: u32, cid: u32) -> Self {
        Self::Vsock { api_uds, port, cid }
    }

    /// Fake transport endpoint; crate-internal constructor only.
    pub(crate) fn fake(stream: DuplexStream) -> Self {
        Self::Fake {
            stream: Mutex::new(Some(stream)),
        }
    }

    /// Replay placeholder until reconcile adopts/kills/quarantines.
    #[must_use]
    pub fn replay_placeholder() -> Self {
        Self::Unresolved
    }
}

/// Framed byte transport abstraction for guest links.
#[async_trait]
pub trait GuestByteStream: Send {
    async fn write_frame(&mut self, bytes: &[u8]) -> Result<(), GuestLinkError>;
    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, GuestLinkError>;
}

/// Errors surfacing at the guest transport layer.
#[derive(Debug, thiserror::Error)]
pub enum GuestLinkError {
    #[error("io: {0}")]
    Io(String),
    #[error("protocol: {0}")]
    Protocol(String),
}

/// A duplex byte pipe (in-process fake transport).
pub struct DuplexStream {
    read_half: ReadHalf<tokio::io::DuplexStream>,
    write_half: WriteHalf<tokio::io::DuplexStream>,
    len_buf: [u8; 4],
    frame_buf: Vec<u8>,
}

/// Create a connected duplex pair for the fake backend: bytes written to
/// side A are read from side B and vice versa.
#[must_use]
pub fn duplex_pair() -> (DuplexStream, DuplexStream) {
    let (w2g_a, w2g_b) = tokio::io::duplex(64 * 1024); // worker -> guest
    let (g2w_a, g2w_b) = tokio::io::duplex(64 * 1024); // guest -> worker
    let (_, w2g_w) = tokio::io::split(w2g_a);
    let (g2w_r, _) = tokio::io::split(g2w_b);
    let (w2g_r, _) = tokio::io::split(w2g_b);
    let (_, g2w_w) = tokio::io::split(g2w_a);
    (
        DuplexStream {
            read_half: g2w_r,
            write_half: w2g_w,
            len_buf: [0; 4],
            frame_buf: Vec::new(),
        },
        DuplexStream {
            read_half: w2g_r,
            write_half: g2w_w,
            len_buf: [0; 4],
            frame_buf: Vec::new(),
        },
    )
}

impl tokio::io::AsyncRead for DuplexStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.read_half).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for DuplexStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.write_half).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write_half).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write_half).poll_shutdown(cx)
    }
}

impl std::fmt::Debug for DuplexStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplexStream").finish_non_exhaustive()
    }
}

#[async_trait]
impl GuestByteStream for DuplexStream {
    async fn write_frame(&mut self, bytes: &[u8]) -> Result<(), GuestLinkError> {
        self.write_half
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .await
            .map_err(|e| GuestLinkError::Io(e.to_string()))?;
        self.write_half
            .write_all(bytes)
            .await
            .map_err(|e| GuestLinkError::Io(e.to_string()))?;
        self.write_half
            .flush()
            .await
            .map_err(|e| GuestLinkError::Io(e.to_string()))
    }

    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, GuestLinkError> {
        match self.read_half.read_exact(&mut self.len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(GuestLinkError::Io(e.to_string())),
        }
        let len = u32::from_le_bytes(self.len_buf) as usize;
        if len > limits::MAX_FRAME_BYTES {
            return Err(GuestLinkError::Protocol("frame too large".into()));
        }
        self.frame_buf.clear();
        self.frame_buf.resize(len, 0);
        self.read_half
            .read_exact(&mut self.frame_buf)
            .await
            .map_err(|e| GuestLinkError::Io(e.to_string()))?;
        Ok(Some(std::mem::take(&mut self.frame_buf)))
    }
}

/// A framed bidirectional link to one guest generation.
#[async_trait]
pub trait GuestLink: Send {
    async fn send(&mut self, msg: &WorkerToGuest) -> WorkerResult<()>;
    async fn recv(&mut self) -> WorkerResult<GuestToWorker>;
}

fn header_for(op: &str, execution: Option<&ExecutionId>, seq: u64) -> FrameHeader {
    FrameHeader {
        protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
        request_id: format!("w2g-{seq}"),
        sandbox_id: None,
        instance_generation: None,
        execution_id: execution.cloned(),
        operation: op.to_string(),
        sequence: seq,
        deadline_ms: None,
        payload_len: 0,
        payload_digest: None,
    }
}

fn op_of(msg: &WorkerToGuest) -> Result<String, WorkerError> {
    let v = serde_json::to_value(msg)
        .map_err(|e| WorkerError::Internal(format!("guest msg serialize: {e}")))?;
    v.get("op")
        .and_then(|o| o.as_str())
        .map(str::to_string)
        .ok_or_else(|| WorkerError::Internal("guest message lacks op tag".into()))
}

/// Link over any duplex byte stream (duplex for fake; unix stream for vsock
/// UDS CONNECT; the same code paths serve all of them).
pub struct StreamGuestLink<S> {
    stream: S,
    decoder: FrameDecoder,
    seq: u64,
}

impl<S: AsyncReadExt + AsyncWriteExt + Unpin + Send> StreamGuestLink<S> {
    #[must_use]
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            decoder: FrameDecoder::new(),
            seq: 0,
        }
    }
}

#[async_trait]
impl<S: AsyncReadExt + AsyncWriteExt + Unpin + Send> GuestLink for StreamGuestLink<S> {
    async fn send(&mut self, msg: &WorkerToGuest) -> WorkerResult<()> {
        let op = op_of(msg)?;
        let execution = match msg {
            WorkerToGuest::ExecSpawn { execution_id, .. }
            | WorkerToGuest::ExecCancel { execution_id } => Some(execution_id),
            _ => None,
        };
        self.seq += 1;
        let bytes = encode_frame(header_for(op.as_str(), execution, self.seq), msg, false)?;
        self.stream
            .write_all(&bytes)
            .await
            .map_err(|e| WorkerError::GuestLink(format!("write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| WorkerError::GuestLink(format!("flush: {e}")))?;
        Ok(())
    }

    // Decode-first-then-read: the loop re-enters after topping up the
    // decoder buffer, so `never_loop` is a false positive here.
    #[allow(clippy::never_loop)]
    async fn recv(&mut self) -> WorkerResult<GuestToWorker> {
        loop {
            while let Some(frame) = self
                .decoder
                .next_frame(now_ms())
                .map_err(|e| WorkerError::GuestLink(format!("frame: {e}")))?
            {
                return decode_guest_frame(frame);
            }
            let mut buf = [0u8; 8192];
            let n = self
                .stream
                .read(&mut buf)
                .await
                .map_err(|e| WorkerError::GuestLink(format!("read: {e}")))?;
            if n == 0 {
                return Err(WorkerError::GuestLink("guest closed stream".into()));
            }
            self.decoder.push(&buf[..n]);
        }
    }
}

fn decode_guest_frame(frame: WireFrame) -> WorkerResult<GuestToWorker> {
    serde_json::from_value(frame.body)
        .map_err(|e| WorkerError::GuestLink(format!("guest message malformed: {e}")))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Factory producing guest links. Injectable so the fake backend (and the
/// firecracker crate) supply their own transports.
#[async_trait]
pub trait GuestLinkFactory: Send + Sync {
    async fn connect(
        &self,
        sandbox_id: &SandboxId,
        generation: InstanceGeneration,
        endpoint: &GuestEndpoint,
    ) -> WorkerResult<Box<dyn GuestLink>>;
}

/// Default factory: fake endpoints take their registered duplex stream;
/// vsock endpoints use the jailer UDS CONNECT handshake or AF_VSOCK (Linux).
#[derive(Debug, Default)]
pub struct DefaultGuestLinkFactory;

#[async_trait]
impl GuestLinkFactory for DefaultGuestLinkFactory {
    async fn connect(
        &self,
        _sandbox_id: &SandboxId,
        _generation: InstanceGeneration,
        endpoint: &GuestEndpoint,
    ) -> WorkerResult<Box<dyn GuestLink>> {
        match endpoint {
            GuestEndpoint::Fake { stream } => {
                let taken = stream
                    .lock()
                    .map_err(|_| WorkerError::GuestLink("endpoint lock poisoned".into()))?
                    .take();
                match taken {
                    Some(duplex) => Ok(Box::new(StreamGuestLink::new(duplex))),
                    None => Err(WorkerError::GuestLink(
                        "fake guest stream already consumed".into(),
                    )),
                }
            }
            GuestEndpoint::Vsock { api_uds, port, cid } => {
                if api_uds.as_os_str().is_empty() {
                    let s = VsockStream::connect(*cid, *port)?;
                    Ok(Box::new(StreamGuestLink::new(s)))
                } else {
                    let mut s = tokio::net::UnixStream::connect(api_uds)
                        .await
                        .map_err(|e| WorkerError::GuestLink(format!("vsock uds: {e}")))?;
                    // Firecracker vsock UDS CONNECT handshake.
                    let line = format!("CONNECT {port}\n");
                    s.write_all(line.as_bytes())
                        .await
                        .map_err(|e| WorkerError::GuestLink(format!("vsock connect line: {e}")))?;
                    let mut ok = [0u8; 2];
                    s.read_exact(&mut ok)
                        .await
                        .map_err(|e| WorkerError::GuestLink(format!("vsock ack: {e}")))?;
                    if &ok != b"OK" {
                        return Err(WorkerError::GuestLink("vsock CONNECT refused".into()));
                    }
                    Ok(Box::new(StreamGuestLink::new(s)))
                }
            }
            GuestEndpoint::Unresolved => Err(WorkerError::GuestLink(
                "guest endpoint unresolved for this generation (reconcile required)".into(),
            )),
        }
    }
}

/// AF_VSOCK stream (Linux) via libc + tokio `AsyncFd`.
#[cfg(target_os = "linux")]
pub struct VsockStream {
    inner: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
}

#[cfg(target_os = "linux")]
impl VsockStream {
    /// Connect to `cid:port` over AF_VSOCK.
    pub fn connect(cid: u32, port: u32) -> WorkerResult<Self> {
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(WorkerError::GuestLink(format!(
                "vsock socket: {}",
                std::io::Error::last_os_error()
            )));
        }
        let addr = SockaddrVm {
            svm_family: libc::AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: cid,
        };
        let rc = unsafe {
            libc::connect(
                fd,
                &addr as *const SockaddrVm as *const libc::sockaddr,
                std::mem::size_of::<SockaddrVm>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            unsafe { libc::close(fd) };
            return Err(WorkerError::GuestLink(format!(
                "vsock connect cid {cid} port {port}: {}",
                std::io::Error::last_os_error()
            )));
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            unsafe { libc::close(fd) };
            return Err(WorkerError::GuestLink(format!(
                "vsock nonblock: {}",
                std::io::Error::last_os_error()
            )));
        }
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let inner = tokio::io::unix::AsyncFd::new(owned)
            .map_err(|e| WorkerError::GuestLink(format!("vsock register: {e}")))?;
        Ok(Self { inner })
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncRead for VsockStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf,
    ) -> std::task::Poll<std::io::Result<()>> {
        loop {
            let mut guard = std::task::ready!(self.inner.poll_read_ready(cx))?;
            let fd = std::os::fd::AsRawFd::as_raw_fd(guard.get_ref());
            let filled = unsafe {
                libc::read(
                    fd,
                    buf.unfilled_mut() as *mut _ as *mut libc::c_void,
                    buf.remaining(),
                )
            };
            if filled < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                return std::task::Poll::Ready(Err(err));
            }
            guard.clear_ready();
            unsafe { buf.assume_init(filled as usize) };
            buf.advance(filled as usize);
            return std::task::Poll::Ready(Ok(()));
        }
    }
}

#[cfg(target_os = "linux")]
impl tokio::io::AsyncWrite for VsockStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        loop {
            let mut guard = std::task::ready!(self.inner.poll_write_ready(cx))?;
            let fd = std::os::fd::AsRawFd::as_raw_fd(guard.get_ref());
            let written =
                unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
            if written < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                return std::task::Poll::Ready(Err(err));
            }
            guard.clear_ready();
            return std::task::Poll::Ready(Ok(written as usize));
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Non-Linux fallback: vsock is unavailable; callers surface an error.
#[cfg(not(target_os = "linux"))]
pub struct VsockStream;

#[cfg(not(target_os = "linux"))]
impl VsockStream {
    pub fn connect(_cid: u32, _port: u32) -> WorkerResult<Self> {
        Err(WorkerError::GuestLink(
            "AF_VSOCK is only supported on Linux".into(),
        ))
    }
}

/// In-process guest simulator for the fake backend and contract tests.
/// Speaks the real `WorkerToGuest`/`GuestToWorker` catalogs with the real
/// boot-nonce handshake. All guest-claimed state here is scripted: the
/// worker side must (and does) never treat it as authority (S17).
pub struct SimGuest {
    stream: DuplexStream,
    decoder: FrameDecoder,
    seq: u64,
    behavior: SimBehavior,
}

/// Scripted behavior for a [`SimGuest`].
#[derive(Debug, Clone)]
pub struct SimBehavior {
    /// Bytes reported as the exec's stdout chunk (before exit).
    pub stdout: Vec<u8>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    /// Echo a garbage nonce on `Hello` instead of the worker's challenge
    /// (negative association test).
    pub wrong_nonce: bool,
    /// Refuse to exit on cancel (execution stays reported-live).
    pub ignore_cancel: bool,
    /// Crash right after spawn: Accepted/Spawned events never arrive, but
    /// the guest still ran the process (cancel can still converge it).
    pub drop_accepted: bool,
    /// Crash right before the terminal event: the process ran to completion
    /// (the sim records it exited, so cancel is a no-op) yet `Exited` is
    /// never delivered — the host must treat absence of `exited` as
    /// non-terminal, never as success (§6.4, T19).
    pub drop_exit: bool,
    /// Guest-initiated broker requests issued right after the handshake
    /// (§9 e2e tests).
    pub broker_requests: Vec<agentkit_sandbox_contracts::api::PackageFetchRequest>,
    /// Mirror of every `WorkerToGuest` frame the simulator receives
    /// (tests observe broker chunks/denials through this).
    pub capture: Option<tokio::sync::mpsc::UnboundedSender<WorkerToGuest>>,
}

impl Default for SimBehavior {
    fn default() -> Self {
        Self {
            stdout: b"hello from guest".to_vec(),
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            wrong_nonce: false,
            ignore_cancel: false,
            broker_requests: Vec::new(),
            capture: None,
            drop_accepted: false,
            drop_exit: false,
        }
    }
}

impl SimGuest {
    /// New simulator over the guest side of a [`duplex_pair`].
    #[must_use]
    pub fn new(guest_side: DuplexStream) -> Self {
        Self {
            stream: guest_side,
            decoder: FrameDecoder::new(),
            seq: 0,
            behavior: SimBehavior::default(),
        }
    }

    #[must_use]
    pub fn with_behavior(mut self, behavior: SimBehavior) -> Self {
        self.behavior = behavior;
        self
    }

    /// Serve until the worker closes the link or shutdown is requested.
    pub async fn serve(mut self) {
        // Filesystem image: guest path -> bytes (workspace semantics only).
        let mut files: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        // executions that already exited (cancel is then a no-op)
        let mut exited: std::collections::HashSet<String> = std::collections::HashSet::new();
        // streaming staging sessions (§11.1/§11.2) — contract-test twin of
        // the guest agent's staging state machine.
        let mut staging = SimStaging::default();
        loop {
            let msg = match self.guest_recv().await {
                Ok(m) => m,
                Err(_) => return,
            };
            if let Some(tx) = &self.behavior.capture {
                let _ = tx.send(msg.clone());
            }
            match msg {
                WorkerToGuest::HelloAck {
                    boot_nonce_b64,
                    protocol_version,
                    ..
                } => {
                    let echo = if self.behavior.wrong_nonce {
                        "bm90LXRoZS1ub25jZQ==".to_string()
                    } else {
                        boot_nonce_b64
                    };
                    let hello = GuestToWorker::Hello {
                        boot_nonce_b64: echo,
                        guest_agent_version: "sim-1".into(),
                        protocol_min: protocol_version,
                        protocol_max: protocol_version,
                        capabilities: vec![
                            "search".into(),
                            "git".into(),
                            "pip".into(),
                            "npm".into(),
                            "cargo".into(),
                        ],
                    };
                    if self.guest_send(&hello).await.is_err() {
                        return;
                    }
                    for fetch in self.behavior.broker_requests.clone() {
                        let req = GuestToWorker::BrokerRequest {
                            request_id: format!("br-{}", self.seq),
                            fetch,
                        };
                        if self.guest_send(&req).await.is_err() {
                            return;
                        }
                    }
                }
                WorkerToGuest::ExecSpawn {
                    execution_id, argv, ..
                } => {
                    let exe = match &argv {
                        agentkit_sandbox_contracts::api::ExecArgv::Direct {
                            executable, ..
                        } => executable.clone(),
                        agentkit_sandbox_contracts::api::ExecArgv::Shell { command, .. } => {
                            command.clone()
                        }
                    };
                    let mut out = if self.behavior.stdout.is_empty() {
                        Vec::new()
                    } else {
                        let mut v = self.behavior.stdout.clone();
                        v.extend_from_slice(format!(" <{exe}>").as_bytes());
                        v
                    };
                    if out.is_empty() {
                        out = self.behavior.stdout.clone();
                    }
                    let mut events = Vec::new();
                    if !self.behavior.drop_accepted {
                        events.push(GuestToWorker::ExecEvent {
                            execution_id: execution_id.clone(),
                            sequence: 1,
                            kind: ExecEventKind::Accepted { at_ms: now_ms() },
                        });
                        events.push(GuestToWorker::ExecEvent {
                            execution_id: execution_id.clone(),
                            sequence: 2,
                            kind: ExecEventKind::Spawned {
                                at_ms: now_ms(),
                                guest_pid: 4242,
                            },
                        });
                    }
                    if !self.behavior.drop_accepted && !self.behavior.drop_exit {
                        events.push(GuestToWorker::ExecEvent {
                            execution_id: execution_id.clone(),
                            sequence: 3,
                            kind: ExecEventKind::StdoutChunk {
                                bytes_b64: b64(&out),
                            },
                        });
                    }
                    if !self.behavior.drop_exit && !self.behavior.drop_accepted {
                        events.push(GuestToWorker::ExecEvent {
                            execution_id: execution_id.clone(),
                            sequence: 4,
                            kind: ExecEventKind::Exited {
                                exit_code: self.behavior.exit_code,
                                signal: self.behavior.signal,
                                timed_out: self.behavior.timed_out,
                                at_ms: now_ms(),
                            },
                        });
                    }
                    // A lost terminal observation (`drop_exit`) means the
                    // process physically completed, so cancel is a no-op.
                    // A lost spawn (`drop_accepted`) models a supervisor
                    // crash: the process is still live, so cancel must be
                    // able to converge it (§8.3).
                    if !self.behavior.drop_accepted {
                        exited.insert(execution_id.as_str().to_string());
                    }
                    for e in events {
                        if self.guest_send(&e).await.is_err() {
                            return;
                        }
                    }
                }
                WorkerToGuest::ExecCancel { execution_id } => {
                    if !self.behavior.ignore_cancel && !exited.contains(execution_id.as_str()) {
                        let e = GuestToWorker::ExecEvent {
                            execution_id,
                            sequence: 99,
                            kind: ExecEventKind::Exited {
                                exit_code: None,
                                signal: Some(9),
                                timed_out: false,
                                at_ms: now_ms(),
                            },
                        };
                        if self.guest_send(&e).await.is_err() {
                            return;
                        }
                    }
                }
                WorkerToGuest::FsOp { operation } => {
                    let outcome = sim_fs_result(&operation, &mut files, &mut staging);
                    let reply = GuestToWorker::FsResult {
                        request_id: "sim".into(),
                        outcome,
                    };
                    if self.guest_send(&reply).await.is_err() {
                        return;
                    }
                }
                WorkerToGuest::HealthPing {} => {
                    let h = GuestToWorker::Health {
                        uptime_ms: now_ms(),
                        load_permille: 10,
                    };
                    if self.guest_send(&h).await.is_err() {
                        return;
                    }
                }
                WorkerToGuest::Shutdown { .. } => return,
                WorkerToGuest::BrokerChunk { .. } | WorkerToGuest::BrokerDeny { .. } => {
                    // Broker streaming to the guest: not exercised by the
                    // sim; ignore honestly (no fake success).
                }
            }
        }
    }

    async fn guest_send(&mut self, msg: &GuestToWorker) -> WorkerResult<()> {
        self.seq += 1;
        let v = serde_json::to_value(msg)
            .map_err(|e| WorkerError::Internal(format!("sim msg serialize: {e}")))?;
        let op = v
            .get("op")
            .and_then(|o| o.as_str())
            .unwrap_or("guest")
            .to_string();
        let execution = match msg {
            GuestToWorker::ExecEvent { execution_id, .. } => Some(execution_id),
            _ => None,
        };
        let header = FrameHeader {
            protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
            request_id: format!("g2w-{}", self.seq),
            sandbox_id: None,
            instance_generation: None,
            execution_id: execution.cloned(),
            operation: op,
            sequence: self.seq,
            deadline_ms: None,
            payload_len: 0,
            payload_digest: None,
        };
        let bytes = encode_frame(header, msg, false)?;
        self.stream
            .write_all(&bytes)
            .await
            .map_err(|e| WorkerError::GuestLink(format!("sim write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| WorkerError::GuestLink(format!("sim flush: {e}")))?;
        Ok(())
    }

    #[allow(clippy::never_loop)] // decoder-topping-up loop (see recv)
    async fn guest_recv(&mut self) -> WorkerResult<WorkerToGuest> {
        loop {
            while let Some(frame) = self
                .decoder
                .next_frame(now_ms())
                .map_err(|e| WorkerError::GuestLink(format!("frame: {e}")))?
            {
                return serde_json::from_value(frame.body)
                    .map_err(|e| WorkerError::GuestLink(format!("worker msg: {e}")));
            }
            let mut buf = [0u8; 8192];
            let n = self
                .stream
                .read(&mut buf)
                .await
                .map_err(|e| WorkerError::GuestLink(format!("sim read: {e}")))?;
            if n == 0 {
                return Err(WorkerError::GuestLink("worker closed stream".into()));
            }
            self.decoder.push(&buf[..n]);
        }
    }
}

/// In-memory staging sessions mirroring the real guest agent's state
/// machine (L1 contract tests only).
#[derive(Default)]
pub struct SimStaging {
    active_import: Option<SimImportSession>,
    last_export: Option<(
        agentkit_sandbox_contracts::ids::StageId,
        agentkit_sandbox_contracts::api::StageManifest,
    )>,
}

#[derive(Debug)]
pub struct SimImportSession {
    stage_id: agentkit_sandbox_contracts::ids::StageId,
    expected: Vec<agentkit_sandbox_contracts::api::StageFile>,
    bufs: std::collections::HashMap<String, (Vec<u8>, u32)>,
}

const SIM_CHUNK_BYTES: usize = agentkit_sandbox_contracts::limits::MAX_STAGE_CHUNK_BYTES;

fn sim_fs_result(
    op: &agentkit_sandbox_contracts::api::messages::GuestFsOp,
    files: &mut std::collections::HashMap<String, Vec<u8>>,
    staging: &mut SimStaging,
) -> GuestOpOutcome {
    use agentkit_sandbox_contracts::api::messages::GuestFsOp;
    match op {
        GuestFsOp::Read { path, from, len } => match files.get(path.as_str()) {
            None => GuestOpOutcome::Failed {
                code: "not_found".into(),
                message: "no such file".into(),
            },
            Some(data) => {
                let from = from.unwrap_or(0) as usize;
                let total = data.len();
                let from = from.min(total);
                let want = len.map(|l| l as usize).unwrap_or(total - from);
                let end = (from + want).min(total);
                let slice = &data[from..end];
                GuestOpOutcome::ReadOk {
                    reply: agentkit_sandbox_contracts::api::ReadReply {
                        data_b64: b64(slice),
                        sha256: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(slice),
                        total_size: total as u64,
                        from: from as u64,
                        next_from: if end < total { Some(end as u64) } else { None },
                        truncated: end < total,
                        modified_ms: Some(now_ms()),
                    },
                }
            }
        },
        GuestFsOp::Write {
            path,
            body_b64,
            expected_sha256,
            ..
        } => {
            let body = match unb64(body_b64) {
                Ok(b) => b,
                Err(_) => {
                    return GuestOpOutcome::Failed {
                        code: "invalid_path".into(),
                        message: "body not base64".into(),
                    };
                }
            };
            let digest = agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(&body);
            if let Some(exp) = expected_sha256
                && *exp != digest
            {
                return GuestOpOutcome::Failed {
                    code: "digest_mismatch".into(),
                    message: "expected digest mismatch".into(),
                };
            }
            let created = !files.contains_key(path.as_str());
            files.insert(path.as_str().to_string(), body.clone());
            GuestOpOutcome::WriteOk {
                receipt: agentkit_sandbox_contracts::api::WriteReceipt {
                    path: path.clone(),
                    sha256: digest,
                    bytes_written: body.len() as u64,
                    created,
                },
            }
        }
        GuestFsOp::List { path, options } => {
            let prefix = format!("{}/", path.as_str().trim_end_matches('/'));
            let mut entries: Vec<agentkit_sandbox_contracts::api::ListEntry> = files
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(k, v)| {
                    let name = k.strip_prefix(&prefix).unwrap_or(k).to_string();
                    agentkit_sandbox_contracts::api::ListEntry {
                        name,
                        kind: agentkit_sandbox_contracts::api::EntryKind::File,
                        size: v.len() as u64,
                    }
                })
                .collect();
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let truncated = entries.len() > options.max_entries as usize;
            entries.truncate(options.max_entries as usize);
            GuestOpOutcome::ListOk {
                reply: agentkit_sandbox_contracts::api::ListReply { entries, truncated },
            }
        }
        GuestFsOp::Search { query } => {
            let mut matches = Vec::new();
            let mut scanned = 0u32;
            for (k, v) in files.iter() {
                if !k.starts_with(query.root.as_str()) {
                    continue;
                }
                scanned += 1;
                let text = String::from_utf8_lossy(v).to_string();
                for (idx, line) in text.lines().enumerate() {
                    let hit = if query.literal {
                        line.contains(&query.pattern)
                    } else {
                        line.to_lowercase().contains(&query.pattern.to_lowercase())
                    };
                    if hit {
                        matches.push(agentkit_sandbox_contracts::api::SearchMatch {
                            path: k.clone(),
                            line: idx as u32 + 1,
                            text: line.to_string(),
                            context_before: Vec::new(),
                            context_after: Vec::new(),
                        });
                        if matches.len() >= query.max_matches as usize {
                            break;
                        }
                    }
                }
            }
            GuestOpOutcome::SearchOk {
                reply: agentkit_sandbox_contracts::api::SearchReply {
                    matches,
                    files_scanned: scanned,
                    truncated: false,
                    timed_out: false,
                },
            }
        }
        GuestFsOp::GitInspect { query } => {
            let text = format!("sim git {:?} {}", query.command, query.repo.as_str());
            GuestOpOutcome::GitOk {
                reply: agentkit_sandbox_contracts::api::GitInspectReply {
                    sha256: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(
                        text.as_bytes(),
                    ),
                    text,
                    truncated: false,
                    exit_code: 0,
                },
            }
        }
        GuestFsOp::ImportBegin { stage_id, files } => {
            staging.active_import = None;
            for f in files {
                if let Err(e) =
                    agentkit_sandbox_contracts::api::validate_stage_relative_path(&f.relative_path)
                {
                    return GuestOpOutcome::Failed {
                        code: "invalid_path".into(),
                        message: e,
                    };
                }
            }
            staging.active_import = Some(SimImportSession {
                stage_id: stage_id.clone(),
                expected: files.clone(),
                bufs: std::collections::HashMap::new(),
            });
            GuestOpOutcome::Ack {}
        }
        GuestFsOp::ImportData {
            stage_id,
            relative_path,
            sequence,
            bytes_b64,
            chunk_digest,
        } => {
            let Some(session) = staging.active_import.as_mut() else {
                return GuestOpOutcome::Failed {
                    code: "not_found".into(),
                    message: "no active import (begin first)".into(),
                };
            };
            if &session.stage_id != stage_id {
                return GuestOpOutcome::Failed {
                    code: "invalid_path".into(),
                    message: "stage id mismatch".into(),
                };
            }
            let Some(spec) = session
                .expected
                .iter()
                .find(|f| &f.relative_path == relative_path)
            else {
                return GuestOpOutcome::Failed {
                    code: "not_found".into(),
                    message: "file not in import manifest".into(),
                };
            };
            let bytes = match unb64(bytes_b64) {
                Ok(b) => b,
                Err(_) => {
                    return GuestOpOutcome::Failed {
                        code: "corrupt".into(),
                        message: "bad base64".into(),
                    };
                }
            };
            if bytes.len() > SIM_CHUNK_BYTES
                || agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(&bytes)
                    != *chunk_digest
            {
                return GuestOpOutcome::Failed {
                    code: "digest_mismatch".into(),
                    message: "chunk digest mismatch".into(),
                };
            }
            let entry = session
                .bufs
                .entry(relative_path.clone())
                .or_insert_with(|| (Vec::new(), 0));
            if entry.1 != *sequence {
                return GuestOpOutcome::Failed {
                    code: "sequence".into(),
                    message: "chunk sequence gap".into(),
                };
            }
            if entry.0.len() + bytes.len() > spec.size as usize {
                return GuestOpOutcome::Failed {
                    code: "too_large".into(),
                    message: "file overran manifest size".into(),
                };
            }
            entry.0.extend_from_slice(&bytes);
            entry.1 += 1;
            GuestOpOutcome::Ack {}
        }
        GuestFsOp::ImportEnd { stage_id } => {
            let Some(session) = staging.active_import.take() else {
                return GuestOpOutcome::Failed {
                    code: "not_found".into(),
                    message: "no active import".into(),
                };
            };
            if &session.stage_id != stage_id {
                return GuestOpOutcome::Failed {
                    code: "invalid_path".into(),
                    message: "stage id mismatch".into(),
                };
            }
            let mut copied = 0u32;
            for f in &session.expected {
                let Some((buf, _seq)) = session.bufs.get(&f.relative_path) else {
                    return GuestOpOutcome::Failed {
                        code: "corrupt".into(),
                        message: format!("missing chunks for {}", f.relative_path),
                    };
                };
                if buf.len() as u64 != f.size
                    || agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(buf) != f.sha256
                {
                    return GuestOpOutcome::Failed {
                        code: "digest_mismatch".into(),
                        message: format!("final digest mismatch for {}", f.relative_path),
                    };
                }
                files.insert(format!("/workspace/{}", f.relative_path), buf.clone());
                copied += 1;
            }
            GuestOpOutcome::ImportOk { copied }
        }
        GuestFsOp::ExportGather { stage_id, paths } => {
            let mut files_out = Vec::new();
            let mut total = 0u64;
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for p in paths {
                let rel = p
                    .as_str()
                    .trim_start_matches('/')
                    .strip_prefix("workspace/")
                    .unwrap_or(p.as_str())
                    .to_string();
                if let Err(e) = agentkit_sandbox_contracts::api::validate_stage_relative_path(&rel)
                {
                    return GuestOpOutcome::Failed {
                        code: "invalid_path".into(),
                        message: e,
                    };
                }
                let Some(body) = files.get(p.as_str()) else {
                    return GuestOpOutcome::Failed {
                        code: "not_found".into(),
                        message: format!("no such file: {p}"),
                    };
                };
                if !seen.insert(rel.clone()) {
                    continue;
                }
                total += body.len() as u64;
                files_out.push(agentkit_sandbox_contracts::api::StageFile {
                    relative_path: rel,
                    sha256: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(body),
                    size: body.len() as u64,
                    mime: "application/octet-stream".into(),
                    provenance: None,
                });
            }
            files_out.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
            let mut manifest = agentkit_sandbox_contracts::api::StageManifest {
                stage_id: stage_id.clone(),
                direction: agentkit_sandbox_contracts::api::StageDirection::Export,
                files: files_out,
                total_bytes: total,
                manifest_digest: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(
                    b"placeholder",
                ),
            };
            manifest.manifest_digest =
                agentkit_sandbox_contracts::canonical::manifest_digest_of(&manifest)
                    .expect("manifest canonicalizes");
            staging.last_export = Some((stage_id.clone(), manifest.clone()));
            GuestOpOutcome::ExportOk { manifest }
        }
        GuestFsOp::ExportFetch {
            stage_id,
            relative_path,
            sequence,
        } => {
            let Some((sid, manifest)) = staging.last_export.clone() else {
                return GuestOpOutcome::Failed {
                    code: "not_found".into(),
                    message: "no gathered export".into(),
                };
            };
            if &sid != stage_id {
                return GuestOpOutcome::Failed {
                    code: "invalid_path".into(),
                    message: "stage id mismatch".into(),
                };
            }
            let Some(spec) = manifest
                .files
                .iter()
                .find(|f| &f.relative_path == relative_path)
            else {
                return GuestOpOutcome::Failed {
                    code: "not_found".into(),
                    message: "path not in export manifest".into(),
                };
            };
            let key = format!("/workspace/{}", relative_path);
            let Some(body) = files.get(&key) else {
                return GuestOpOutcome::Failed {
                    code: "not_found".into(),
                    message: "file vanished after gather".into(),
                };
            };
            if agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(body) != spec.sha256
                || body.len() as u64 != spec.size
            {
                return GuestOpOutcome::Failed {
                    code: "digest_mismatch".into(),
                    message: "file mutated after gather".into(),
                };
            }
            let start = (*sequence as usize).saturating_mul(SIM_CHUNK_BYTES);
            if start >= body.len() {
                return GuestOpOutcome::Failed {
                    code: "invalid_path".into(),
                    message: "sequence past end".into(),
                };
            }
            let end = (start + SIM_CHUNK_BYTES).min(body.len());
            let chunk = &body[start..end];
            GuestOpOutcome::ExportDataOk {
                bytes_b64: b64(chunk),
                chunk_digest: agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(chunk),
                last: end == body.len(),
            }
        }
    }
}

/// Base64 (standard, padded) — staging/handshake wire encoding.
pub(crate) fn b64(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    STANDARD.encode(bytes)
}

pub(crate) fn unb64(s: &str) -> Result<Vec<u8>, WorkerError> {
    use base64::engine::general_purpose::STANDARD;
    STANDARD
        .decode(s)
        .map_err(|e| WorkerError::Internal(format!("base64: {e}")))
}

/// Worker-side boot-nonce binding: the guest's echoed nonce must hash to the
/// exact digest the backend committed at launch (T03 evidence).
pub(crate) fn verify_boot_nonce(echo_b64: &str, expected: &str) -> Result<(), WorkerError> {
    let raw = unb64(echo_b64)?;
    if raw.len() != 32 {
        return Err(WorkerError::GuestLink("boot nonce not 32 bytes".into()));
    }
    let want = agentkit_sandbox_contracts::ids::ArtifactDigest::from_hex(
        expected.trim_start_matches("sha256:"),
    )
    .map_err(|e| WorkerError::Internal(format!("nonce digest shape: {e}")))?;
    if agentkit_sandbox_contracts::ids::ArtifactDigest::of_bytes(&raw) != want {
        return Err(WorkerError::GuestLink(
            "boot nonce does not match launch commitment".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_sandbox_contracts::PROTOCOL_VERSION;
    use agentkit_sandbox_contracts::ids::ArtifactDigest;
    use agentkit_sandbox_contracts::path::GuestPath;

    #[tokio::test]
    async fn handshake_completes() {
        let (w, gs) = duplex_pair();
        let mut link = StreamGuestLink::new(w);
        let sim = SimGuest::new(gs);
        let handle = tokio::spawn(sim.serve());
        let nonce = [7u8; 32];
        link.send(&worker_hello(&nonce)).await.unwrap();
        let msg = link.recv().await.unwrap();
        match msg {
            GuestToWorker::Hello {
                boot_nonce_b64,
                protocol_min,
                protocol_max,
                ..
            } => {
                assert_eq!(protocol_min, PROTOCOL_VERSION);
                assert_eq!(protocol_max, PROTOCOL_VERSION);
                verify_boot_nonce(
                    &boot_nonce_b64,
                    &ArtifactDigest::of_bytes(&nonce).to_string(),
                )
                .unwrap();
            }
            other => panic!("expected hello, got {other:?}"),
        }
        drop(link);
        let _ = handle.await;
    }

    fn worker_hello(nonce: &[u8]) -> WorkerToGuest {
        WorkerToGuest::HelloAck {
            session_digest: ArtifactDigest::of_bytes(b"session"),
            protocol_version: PROTOCOL_VERSION,
            boot_nonce_b64: b64(nonce),
            workspace_root: GuestPath::parse("/workspace").unwrap(),
        }
    }

    #[tokio::test]
    async fn wrong_echoed_nonce_is_rejected() {
        let (w, gs) = duplex_pair();
        let mut link = StreamGuestLink::new(w);
        let sim = SimGuest::new(gs).with_behavior(SimBehavior {
            wrong_nonce: true,
            ..Default::default()
        });
        let handle = tokio::spawn(sim.serve());
        link.send(&worker_hello(&[9u8; 32])).await.unwrap();
        let msg = link.recv().await.unwrap();
        let GuestToWorker::Hello { boot_nonce_b64, .. } = msg else {
            panic!("expected hello")
        };
        // Simulated guest echoes garbage: binding check must fail closed.
        let digest_of_real = ArtifactDigest::of_bytes(&[9u8; 32]).to_string();
        assert!(verify_boot_nonce(&boot_nonce_b64, &digest_of_real).is_err());
        drop(link);
        let _ = handle.await;
    }
}
