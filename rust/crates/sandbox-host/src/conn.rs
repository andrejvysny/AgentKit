//! Framed UDS connection engine: connect + peer-auth + Hello handshake,
//! per-direction sequence guard, request-id correlation to per-request
//! oneshots.
//!
//! Wire format (mirrors `agentkit_sandbox_worker::transport`):
//! `[u32 LE length][WireFrame JSON]`, header validated by
//! `agentkit_sandbox_contracts::protocol`, bodies are the `HostToWorker` /
//! `WorkerToHost` catalogs.

use std::collections::HashMap;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, oneshot};

use agentkit_sandbox_contracts::api::ExecEvent;
use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerToHost};
use agentkit_sandbox_contracts::protocol::{FrameDecoder, FrameHeader, encode_frame};

use crate::client::now_ms;
use crate::client::{ClientBinding, HelloInfo, HostError, WorkerClientConfig};

pub(crate) struct Conn {
    write: Mutex<OwnedWriteHalf>,
    seq: AtomicU64,
    pending: StdMutex<HashMap<String, oneshot::Sender<Result<WorkerToHost, ()>>>>,
    pub(crate) dead: AtomicBool,
    /// Pushed execution events (best effort; authoritative replay is via
    /// the `ExecEvents` request).
    pushed: StdMutex<Vec<ExecEvent>>,
}

impl Conn {
    fn new(write: OwnedWriteHalf) -> Arc<Self> {
        Arc::new(Self {
            write: Mutex::new(write),
            seq: AtomicU64::new(0),
            pending: StdMutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            pushed: StdMutex::new(Vec::new()),
        })
    }

    pub(crate) fn register(&self, id: &str) -> oneshot::Receiver<Result<WorkerToHost, ()>> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(id.to_string(), tx);
        rx
    }

    pub(crate) fn remove(&self, id: &str) {
        self.pending.lock().expect("pending lock").remove(id);
    }

    pub(crate) fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
        let drained: Vec<(String, oneshot::Sender<Result<WorkerToHost, ()>>)> =
            self.pending.lock().expect("pending lock").drain().collect();
        for (_, tx) in drained {
            let _ = tx.send(Err(()));
        }
    }

    pub(crate) fn drain_pushed(&self) -> Vec<ExecEvent> {
        std::mem::take(&mut *self.pushed.lock().expect("pushed lock"))
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) async fn write_frame(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.write.lock().await;
        w.write_all(bytes).await?;
        w.flush().await
    }
}

/// An established association: connection + reader task + HelloAck facts.
pub(crate) struct Session {
    pub(crate) conn: Arc<Conn>,
    pub(crate) reader: tokio::task::JoinHandle<()>,
    pub(crate) hello: HelloInfo,
}

fn route(conn: &Arc<Conn>, msg: WorkerToHost) {
    match &msg {
        WorkerToHost::Reply { request_id, .. } => {
            if let Some(tx) = conn
                .pending
                .lock()
                .expect("pending lock")
                .remove(request_id)
            {
                let _ = tx.send(Ok(msg));
            }
        }
        WorkerToHost::HelloAck { .. } => {
            if let Some(tx) = conn
                .pending
                .lock()
                .expect("pending lock")
                .remove("hello-ack")
            {
                let _ = tx.send(Ok(msg));
            }
        }
        WorkerToHost::PushExecEvent(ev) => {
            let mut q = conn.pushed.lock().expect("pushed lock");
            if q.len() >= agentkit_sandbox_contracts::limits::MAX_BUFFERED_EVENTS {
                q.remove(0);
            }
            q.push(ev.clone());
        }
    }
}

fn spawn_reader(mut read: OwnedReadHalf, conn: Arc<Conn>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    conn.mark_dead();
                    return;
                }
                Ok(n) => {
                    decoder.push(&buf[..n]);
                    loop {
                        match decoder.next_frame(now_ms()) {
                            Ok(Some(frame)) => {
                                match serde_json::from_value::<WorkerToHost>(frame.body) {
                                    Ok(msg) => route(&conn, msg),
                                    Err(_) => {
                                        // Malformed reply stream: terminate
                                        // the connection (no resync, S16).
                                        conn.mark_dead();
                                        return;
                                    }
                                }
                            }
                            Ok(None) => break,
                            Err(_) => {
                                conn.mark_dead();
                                return;
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Connect, peer-auth, and handshake. One attempt; the caller retries at the
/// request level per the mutation-safety policy.
pub(crate) async fn establish(
    config: &WorkerClientConfig,
    binding: &ClientBinding,
    hello_id: String,
) -> Result<Session, HostError> {
    let stream = tokio::time::timeout(config.connect_timeout, UnixStream::connect(&config.uds))
        .await
        .map_err(|_| HostError::Connect(format!("timeout connecting {}", config.uds.display())))?
        .map_err(|e| HostError::Connect(format!("{}: {e}", config.uds.display())))?;
    let uid = peer_uid(&stream)?;
    if uid != config.expected_peer_uid {
        return Err(HostError::PeerAuth {
            expected: config.expected_peer_uid,
            actual: uid,
        });
    }
    let (read_half, write_half) = stream.into_split();
    let conn = Conn::new(write_half);
    // Reader starts first so the ack can arrive immediately.
    let reader = spawn_reader(read_half, Arc::clone(&conn));

    let body = HostToWorker::Hello {
        host_id: config.host_id.clone(),
        protocol_versions: config.protocol_versions.clone(),
    };
    let rx_ack = conn.register("hello-ack");
    let rx_reply = conn.register(&hello_id);
    let header = FrameHeader {
        protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
        request_id: hello_id,
        sandbox_id: Some(binding.sandbox_id.clone()),
        instance_generation: Some(binding.instance_generation),
        execution_id: None,
        operation: "hello".to_string(),
        sequence: conn.next_seq(),
        deadline_ms: Some(now_ms().saturating_add(config.request_timeout.as_millis() as u64)),
        payload_len: 0,
        payload_digest: None,
    };
    let bytes =
        encode_frame(header, &body, false).map_err(|e| HostError::Handshake(e.to_string()))?;
    let write_res = conn.write_frame(&bytes).await;
    let ack = tokio::time::timeout(config.request_timeout, async {
        tokio::select! {
            a = rx_ack => a,
            b = rx_reply => b,
        }
    })
    .await;
    let ack = match (write_res, ack) {
        (Err(e), _) => {
            conn.mark_dead();
            reader.abort();
            return Err(HostError::Handshake(format!("hello write: {e}")));
        }
        (_, Err(_)) => {
            conn.mark_dead();
            reader.abort();
            return Err(HostError::Handshake("hello ack timed out".into()));
        }
        (_, Ok(Err(_))) | (_, Ok(Ok(Err(())))) => {
            conn.mark_dead();
            reader.abort();
            return Err(HostError::Handshake(
                "connection died awaiting hello ack".into(),
            ));
        }
        (_, Ok(Ok(Ok(msg)))) => msg,
    };
    let hello = match ack {
        WorkerToHost::HelloAck {
            identity,
            protocol_version,
            ..
        } => HelloInfo {
            worker_id: identity.worker_id,
            revision: identity.revision,
            install_fingerprint: identity.install_fingerprint,
            protocol_version,
        },
        WorkerToHost::Reply {
            outcome: agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error },
            ..
        } => {
            conn.mark_dead();
            reader.abort();
            return Err(HostError::Handshake(format!(
                "worker rejected hello: {} ({})",
                error.code.as_str(),
                error.message
            )));
        }
        other => {
            conn.mark_dead();
            reader.abort();
            return Err(HostError::Handshake(format!(
                "unexpected hello reply kind: {}",
                match other {
                    WorkerToHost::PushExecEvent(_) => "push_exec_event",
                    _ => "other",
                }
            )));
        }
    };
    Ok(Session {
        conn,
        reader,
        hello,
    })
}

/// `SO_PEERCRED` uid of the connected peer (Linux).
pub(crate) fn peer_uid(stream: &UnixStream) -> Result<u32, HostError> {
    use std::os::unix::io::AsRawFd;
    let mut cred: libc::ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(HostError::Connect(format!(
            "SO_PEERCRED: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(cred.uid)
}

/// Encode one request frame for `conn` (unique `request_id` supplied by the
/// caller; sequence advanced per connection; deadline enforced worker-side).
pub(crate) fn encode_request(
    conn: &Conn,
    config: &WorkerClientConfig,
    binding: &ClientBinding,
    request_id: &str,
    msg: &HostToWorker,
    operation: &str,
    execution_id: Option<agentkit_sandbox_contracts::ids::ExecutionId>,
) -> Result<Vec<u8>, agentkit_sandbox_contracts::error::SandboxError> {
    let header = FrameHeader {
        protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
        request_id: request_id.to_string(),
        sandbox_id: Some(binding.sandbox_id.clone()),
        instance_generation: Some(binding.instance_generation),
        execution_id,
        operation: operation.to_string(),
        sequence: conn.next_seq(),
        deadline_ms: Some(now_ms().saturating_add(config.request_timeout.as_millis() as u64)),
        payload_len: 0,
        payload_digest: None,
    };
    encode_frame(header, msg, false).map_err(|e| {
        agentkit_sandbox_contracts::error::SandboxError::new(
            agentkit_sandbox_contracts::error::SandboxErrorCode::ProtocolIncompatible,
            format!("frame encode: {e}"),
        )
    })
}
