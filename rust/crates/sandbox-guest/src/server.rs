//! Frame loop and per-request dispatch (§6.4). One association at a time;
//! fs operations run on the blocking pool; execution events are drained from
//! the supervisor and pushed with per-execution sequence numbers. All
//! guest→worker frames go through `encode_frame` with `digest_body=true` for
//! byte movers.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agentkit_sandbox_contracts::PROTOCOL_VERSION;
use agentkit_sandbox_contracts::api::messages::{GuestFsOp, GuestOpOutcome};
use agentkit_sandbox_contracts::api::{ExecEventKind, GuestToWorker, WorkerToGuest};
use agentkit_sandbox_contracts::ids::ExecutionId;
use agentkit_sandbox_contracts::protocol::{
    FrameDecoder, FrameHeader, SequenceGuard, encode_frame,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf};
use tokio::sync::{mpsc, oneshot};

use crate::error::{GuestFail, code};
use crate::fsops::GuestFs;
use crate::handshake;
use crate::supervisor::{SpawnRequest, Supervisor};

/// Any duplex transport the server can speak frames over.
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}
pub type BoxedStream = Box<dyn Duplex>;

static OUT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Encode one guest→worker frame. `digest_body=true` for byte movers.
pub fn encode_outbound<T: serde::Serialize>(
    operation: &str,
    body: &T,
    digest_body: bool,
) -> Result<Vec<u8>, GuestFail> {
    let seq = OUT_SEQ.fetch_add(1, Ordering::Relaxed);
    let header = FrameHeader {
        protocol_version: PROTOCOL_VERSION,
        request_id: format!("g-{seq}"),
        sandbox_id: None,
        instance_generation: None,
        execution_id: None,
        operation: operation.to_string(),
        sequence: seq,
        deadline_ms: None,
        payload_len: 0,
        payload_digest: None,
    };
    encode_frame(header, body, digest_body)
        .map_err(|e| GuestFail::new(code::BAD_INPUT, e.to_string()))
}

type Outbound = mpsc::UnboundedSender<Vec<u8>>;

/// Wired-up guest server: roots, supervisor, and the event pump.
pub struct Server {
    pub fs: Arc<GuestFs>,
    pub sup: Arc<Supervisor>,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<(ExecutionId, ExecEventKind)>>,
    boot_nonce: Vec<u8>,
    started: Instant,
}

impl Server {
    #[must_use]
    pub fn new(
        fs: Arc<GuestFs>,
        sup: Arc<Supervisor>,
        events: mpsc::UnboundedReceiver<(ExecutionId, ExecEventKind)>,
        boot_nonce: Vec<u8>,
    ) -> Self {
        Self {
            fs,
            sup,
            events: tokio::sync::Mutex::new(events),
            boot_nonce,
            started: Instant::now(),
        }
    }

    /// Serve associations sequentially until the process is stopped.
    pub async fn accept_loop(self: Arc<Self>, listener: &crate::vsock::Listener) {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let me = Arc::clone(&self);
                    if let Err(e) = me.serve_connection(stream).await {
                        eprintln!("association closed: {}", e.code);
                    }
                }
                Err(e) => {
                    eprintln!("accept failed: {}", e.kind());
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }

    /// One full association: handshake then the dispatch loop.
    pub async fn serve_connection(
        self: Arc<Self>,
        mut stream: BoxedStream,
    ) -> Result<(), GuestFail> {
        let mut decoder = FrameDecoder::new();
        handshake::negotiate(&mut stream, &mut decoder, &self.boot_nonce).await?;

        let (mut rd, mut wr) = tokio::io::split(stream);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let writer = tokio::spawn(async move {
            while let Some(bytes) = out_rx.recv().await {
                if wr.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = wr.flush().await;
        });

        let mut seq_guard = SequenceGuard::new();
        let mut exec_seq: HashMap<ExecutionId, u64> = HashMap::new();
        let inflight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let events = self.events.lock().await;
        tokio::pin!(events);

        let result: Result<(), GuestFail> = loop {
            tokio::select! {
                ev = events.recv() => {
                    match ev {
                        Some((id, kind)) => {
                            let seq = exec_seq.entry(id.clone()).or_insert(0);
                            let frame = GuestToWorker::ExecEvent {
                                execution_id: id,
                                sequence: *seq,
                                kind,
                            };
                            *seq += 1;
                            match encode_outbound("exec_event", &frame, true) {
                                Ok(bytes) => {
                                    if out_tx.send(bytes).is_err() {
                                        break Ok(());
                                    }
                                }
                                Err(e) => break Err(e),
                            }
                        }
                        None => break Ok(()), // supervisor gone: end association
                    }
                }
                frame = handshake::read_frame(&mut rd, &mut decoder) => {
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => break Err(e),
                    };
                    if let Err(e) = seq_guard.accept(frame.header.sequence) {
                        break Err(GuestFail::new(code::BAD_INPUT, e.to_string()));
                    }
                    let body = match serde_json::from_value::<WorkerToGuest>(frame.body) {
                        Ok(b) => b,
                        Err(_) => break Err(GuestFail::new(code::BAD_INPUT, "unknown operation")),
                    };
                    let break_opt = self
                        .dispatch(body, frame.header.request_id, &out_tx, &inflight)
                        .await;
                    if let Some(r) = break_opt {
                        break r;
                    }
                }
            }
        };
        // Drain anything queued, then stop the writer.
        drop(out_tx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(250), writer).await;
        result
    }

    /// Handle one decoded request; `Some(result)` ends the association.
    async fn dispatch(
        self: &Arc<Self>,
        body: WorkerToGuest,
        request_id: String,
        out_tx: &Outbound,
        inflight: &Arc<Mutex<HashSet<String>>>,
    ) -> Option<Result<(), GuestFail>> {
        match body {
            WorkerToGuest::FsOp { operation } => {
                if let GuestFsOp::GitInspect { query } = operation {
                    // Async process op: runs on the reactor, not the blocking pool.
                    let fs = Arc::clone(&self.fs);
                    let rid = request_id.clone();
                    let tx2 = out_tx.clone();
                    tokio::spawn(async move {
                        let outcome = match crate::git::git_inspect(&fs, &query).await {
                            Ok(reply) => GuestOpOutcome::GitOk { reply },
                            Err(f) => GuestOpOutcome::Failed {
                                code: f.code.to_string(),
                                message: f.message,
                            },
                        };
                        let msg = GuestToWorker::FsResult {
                            request_id: rid,
                            outcome,
                        };
                        if let Ok(bytes) = encode_outbound("fs_result", &msg, true) {
                            let _ = tx2.send(bytes);
                        }
                    });
                    return None;
                }
                if !inflight
                    .lock()
                    .map(|mut s| s.insert(request_id.clone()))
                    .unwrap_or(true)
                {
                    return None; // single-flight: duplicate while active
                }
                let fs = Arc::clone(&self.fs);
                let (tx, rx) = oneshot::channel();
                let rid = request_id.clone();
                let rid2 = request_id.clone();
                let inflight2 = Arc::clone(inflight);
                let tx2 = out_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = fs.handle(&operation, &rid);
                    let _ = tx.send(outcome);
                });
                tokio::spawn(async move {
                    if let Ok(outcome) = rx.await {
                        let msg = GuestToWorker::FsResult {
                            request_id: rid2.clone(),
                            outcome,
                        };
                        if let Ok(bytes) = encode_outbound("fs_result", &msg, true) {
                            let _ = tx2.send(bytes);
                        }
                        if let Ok(mut s) = inflight2.lock() {
                            s.remove(&rid2);
                        }
                    } else if let Ok(mut s) = inflight2.lock() {
                        s.remove(&rid2);
                    }
                });
                None
            }
            WorkerToGuest::ExecSpawn {
                execution_id,
                argv,
                cwd,
                environment,
                timeout_sec,
                max_output_bytes,
                max_processes,
                run_as_uid,
                run_as_gid,
            } => {
                let req = SpawnRequest {
                    execution_id: execution_id.clone(),
                    argv,
                    cwd,
                    environment,
                    timeout_sec,
                    max_output_bytes,
                    max_processes,
                    run_as_uid,
                    run_as_gid,
                };
                if let Err(f) = self.sup.spawn(req) {
                    self.sup.report_failure(&execution_id, &f);
                }
                None
            }
            WorkerToGuest::ExecCancel { execution_id } => {
                self.sup.cancel(&execution_id);
                None
            }
            WorkerToGuest::HealthPing {} => {
                let h = GuestToWorker::Health {
                    uptime_ms: handshake::uptime_ms(self.started),
                    load_permille: handshake::load_permille(),
                };
                match encode_outbound("health", &h, false) {
                    Ok(bytes) => {
                        if out_tx.send(bytes).is_err() {
                            return Some(Ok(()));
                        }
                    }
                    Err(e) => return Some(Err(e)),
                }
                None
            }
            WorkerToGuest::BrokerChunk { request_id, .. }
            | WorkerToGuest::BrokerDeny { request_id, .. } => {
                // Broker lands in M4; never half-speak the protocol (S05).
                let msg = GuestToWorker::FsResult {
                    request_id,
                    outcome: GuestOpOutcome::Failed {
                        code: code::unsupported().to_string(),
                        message: "broker not served in M1".into(),
                    },
                };
                match encode_outbound("fs_result", &msg, true) {
                    Ok(bytes) => {
                        if out_tx.send(bytes).is_err() {
                            return Some(Ok(()));
                        }
                    }
                    Err(e) => return Some(Err(e)),
                }
                None
            }
            WorkerToGuest::Shutdown { .. } => Some(Ok(())),
            WorkerToGuest::HelloAck { .. } => {
                Some(Err(GuestFail::new(code::BAD_INPUT, "duplicate hello_ack")))
            }
        }
    }
}

/// Read-side half type alias helper for tests.
pub type ServerReadHalf<S> = ReadHalf<S>;
