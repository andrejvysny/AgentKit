//! Host<->Worker UDS listener: peer-credential admission, framed protocol,
//! per-connection sequence/duplicate guards.
//!
//! The socket lives in a 0700 directory with 0600 permissions; the peer uid
//! must be in the admin-configured allow set (`SO_PEERCRED`). A peer that is
//! not allowed is closed **silently** (no reply, no error) with an audit
//! line. A malformed frame terminates the connection (S16) — no
//! resynchronization inside a bad stream.

use crate::error::WorkerError;
use crate::service::WorkerService;
use crate::util::set_dir_mode;
use agentkit_sandbox_contracts::api::messages::WorkerToHost;
use agentkit_sandbox_contracts::canonical::canonicalize;
use agentkit_sandbox_contracts::ids::RequestDigest;
use agentkit_sandbox_contracts::protocol::{
    DuplicateOutcome, DuplicateTable, FrameDecoder, SequenceGuard,
};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Result of verifying a peer credential (Linux `SO_PEERCRED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

/// Read `SO_PEERCRED` via libc `getsockopt`.
#[cfg(unix)]
pub fn peer_cred(stream: &UnixStream) -> Result<PeerCred, WorkerError> {
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
        return Err(WorkerError::Storage(format!(
            "SO_PEERCRED: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(PeerCred {
        uid: cred.uid,
        gid: cred.gid,
        pid: cred.pid as u32,
    })
}

#[cfg(not(unix))]
pub fn peer_cred(_stream: &UnixStream) -> Result<PeerCred, WorkerError> {
    Err(WorkerError::Storage(
        "peer credentials require a unix platform".into(),
    ))
}

/// Bind the worker UDS: parent directory created 0700, socket file 0600.
pub fn bind_uds(path: &Path) -> Result<UnixListener, WorkerError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| WorkerError::Storage(format!("uds dir: {e}")))?;
        set_dir_mode(parent, 0o700);
    }
    // A stale socket file would block bind; remove only our own path.
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|e| WorkerError::Storage(format!("stale uds unlink: {e}")))?;
    }
    let listener =
        UnixListener::bind(path).map_err(|e| WorkerError::Storage(format!("uds bind: {e}")))?;
    set_dir_mode(path, 0o600);
    Ok(listener)
}

/// Accept loop: peer-credential admission then per-connection service.
pub async fn serve(
    listener: UnixListener,
    service: Arc<WorkerService>,
    peer_uids: Vec<u32>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                tracing_line("shutdown signal: stopping accept loop");
                return;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let svc = Arc::clone(&service);
                        let uids = peer_uids.clone();
                        tokio::spawn(async move {
                            let mut stream = stream;
                            match peer_cred(&stream) {
                                Ok(cred) if uids.contains(&cred.uid) => {
                                    connection(stream, svc).await;
                                }
                                Ok(cred) => {
                                    // Silent close: no reply, just audit.
                                    // Explicit shutdown first so the peer
                                    // observes a clean EOF, not a reset race.
                                    tracing_line(&format!(
                                        "audit: peer uid {} (pid {}) not in allow set; connection closed",
                                        cred.uid, cred.pid
                                    ));
                                    let _ = stream.shutdown().await;
                                    drop(stream);
                                }
                                Err(e) => {
                                    tracing_line(&format!("peer credential lookup failed: {e}; closing"));
                                    let _ = stream.shutdown().await;
                                    drop(stream);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing_line(&format!("accept failed: {e}"));
                        // Transient accept errors must not kill the daemon.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
}

fn tracing_line(msg: &str) {
    eprintln!("audit: {msg}");
}

/// One host connection: decode frames, guard sequence + duplicates, dispatch.
pub async fn connection(stream: UnixStream, service: Arc<WorkerService>) {
    let mut stream = stream;
    let (push_id, mut pushes) = service.register_push();
    let mut decoder = FrameDecoder::new();
    let mut seq_guard = SequenceGuard::new();
    let mut dup = DuplicateTable::new();
    let mut recorded: std::collections::HashMap<String, WorkerToHost> =
        std::collections::HashMap::new();
    let mut buf = [0u8; 16 * 1024];
    let mut out_seq: u64 = 0;
    let result: tokio::io::Result<()> = async {
        loop {
            tokio::select! {
                pushed = pushes.recv() => {
                    match pushed {
                        Some(msg) => {
                            let bytes = reply_bytes(&msg, &mut out_seq)?;
                            stream.write_all(&bytes).await?;
                            stream.flush().await?;
                        }
                        None => break,
                    }
                }
                n = stream.read(&mut buf) => {
                    let n = n?;
                    if n == 0 {
                        break;
                    }
                    decoder.push(&buf[..n]);
                    while let Some(frame) = decoder
                        .next_frame(crate::service::now_ms())
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?
                    {
                        // Monotonic per-direction sequence (S16).
                        seq_guard
                            .accept(frame.header.sequence)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
                        // Duplicate suppression over the canonical body digest.
                        let canonical = canonicalize(&frame.body)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
                        let digest = RequestDigest::of_canonical_json(&canonical);
                        match dup.check(&frame.header.request_id, &digest) {
                            DuplicateOutcome::Replay => {
                                if let Some(prev) = recorded.get(&frame.header.request_id) {
                                    let bytes = reply_bytes(prev, &mut out_seq)?;
                                    stream.write_all(&bytes).await?;
                                    stream.flush().await?;
                                }
                                continue;
                            }
                            DuplicateOutcome::Conflict => {
                                // Conflicting duplicate: protocol violation.
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "duplicate request_id with conflicting payload",
                                ));
                            }
                            DuplicateOutcome::New => {}
                        }
                        let reply = service.handle(&frame).await;
                        dup.record(frame.header.request_id.clone(), digest);
                        recorded.insert(frame.header.request_id, reply.clone());
                        let bytes = reply_bytes(&reply, &mut out_seq)?;
                        stream.write_all(&bytes).await?;
                        stream.flush().await?;
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing_line(&format!("connection terminated: {e}"));
    }
    service.unregister_push(push_id);
}

/// Encode a reply/push as a frame (digest-free; bodies are small and the
/// frame layer re-validates lengths).
fn reply_bytes(msg: &WorkerToHost, out_seq: &mut u64) -> std::io::Result<Vec<u8>> {
    *out_seq += 1;
    let seq = *out_seq;
    let op = match msg {
        WorkerToHost::HelloAck { .. } => "hello_ack",
        WorkerToHost::Reply { .. } => "reply",
        WorkerToHost::PushExecEvent(_) => "push_exec_event",
    };
    let (request_id, execution_id) = match msg {
        WorkerToHost::Reply { request_id, .. } => (request_id.clone(), None),
        WorkerToHost::PushExecEvent(e) => {
            (format!("push-{}", e.sequence), Some(e.execution_id.clone()))
        }
        WorkerToHost::HelloAck { .. } => ("hello-ack".to_string(), None),
    };
    let header = agentkit_sandbox_contracts::protocol::FrameHeader {
        protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
        request_id,
        sandbox_id: None,
        instance_generation: None,
        execution_id,
        operation: op.to_string(),
        sequence: seq,
        deadline_ms: None,
        payload_len: 0,
        payload_digest: None,
    };
    agentkit_sandbox_contracts::protocol::encode_frame(header, msg, false)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}
