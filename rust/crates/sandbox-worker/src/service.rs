//! `HostToWorker` dispatch: the authoritative worker-side request pipeline.
//!
//! Every mutation follows journal-before-effect; every authorization is
//! re-checked server-side against registry truth (grants, envelopes, digests,
//! generations — S02/S08). Wire frames only ever carry guest paths and ids;
//! host paths are derived exclusively from the admin-configured storage root
//! (S20). Guest claims are recorded as reports, never as authority (S17).

use crate::config::WorkerConfig;
use crate::error::{WorkerError, WorkerResult, redact_cap};
use crate::guestlink::{
    GuestEndpoint, GuestLink, GuestLinkFactory, SimGuest, b64, duplex_pair, verify_boot_nonce,
};
use crate::journal::{Journal, JournalRecord};
use crate::registry::{NewVmSpec, QuotaCeilings, Registry};
use agentkit_sandbox_contracts::api::messages::{
    GuestFsOp, GuestOpOutcome, GuestToWorker, HostToWorker, WorkerReplyBody, WorkerToGuest,
    WorkerToHost,
};
use agentkit_sandbox_contracts::api::{
    DeletionReceipt, ExecEvent, ExecEventKind, ExecutionHandle, ReplyOutcomeShape, StageManifest,
    TerminationEvidence, VerifiedLaunchSpec, VmHandle, VmObservedState, VmPhysState,
};
use agentkit_sandbox_contracts::canonical::canonicalize;
use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::grant::{GrantExpectation, WorkspaceGrant};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, InstanceGeneration, RequestDigest, SandboxId, StageId, TenantId,
    WorkerId, WorkerIdentity,
};
use agentkit_sandbox_contracts::limits::{MAX_STAGE_CHUNK_BYTES, MAX_STAGED_TOTAL_BYTES};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::{RegistryPolicy, SandboxCapability};
use agentkit_sandbox_contracts::protocol::WireFrame;
use agentkit_sandbox_contracts::state::SandboxState;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

const GUEST_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Canonical digest bytes over a serializable body (idempotency/duplicate key).
pub fn canonical_body<T: serde::Serialize>(body: &T) -> WorkerResult<Vec<u8>> {
    let value =
        serde_json::to_value(body).map_err(|e| WorkerError::Internal(format!("serialize: {e}")))?;
    canonicalize(&value).map_err(|e| WorkerError::Protocol(format!("canonical: {e}")))
}

fn tracing_line(msg: &str) {
    eprintln!("audit: {msg}");
}

fn guest_op_name(msg: &GuestToWorker) -> String {
    serde_json::to_value(msg)
        .ok()
        .and_then(|v| v.get("op").and_then(|o| o.as_str()).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// A live guest connection: outbound queue + FIFO waiters for serialized
/// fs-results (the wire pairs FsOp→FsResult per-connection in order).
pub struct GuestConn {
    pub generation: InstanceGeneration,
    tx: mpsc::UnboundedSender<WorkerToGuest>,
    pending_fs: Mutex<VecDeque<oneshot::Sender<GuestOpOutcome>>>,
}

impl GuestConn {
    fn request_fs(&self) -> oneshot::Receiver<GuestOpOutcome> {
        let (tx, rx) = oneshot::channel();
        self.pending_fs.lock().expect("fs queue lock").push_back(tx);
        rx
    }

    fn complete_fs(&self, outcome: GuestOpOutcome) {
        if let Some(tx) = self.pending_fs.lock().expect("fs queue lock").pop_front() {
            let _ = tx.send(outcome);
        }
    }

    fn fail_all_fs(&self) {
        let mut q = self.pending_fs.lock().expect("fs queue lock");
        while let Some(tx) = q.pop_front() {
            let _ = tx.send(GuestOpOutcome::Failed {
                code: "link_lost".into(),
                message: "guest link lost".into(),
            });
        }
    }
}

struct ServiceInner {
    journal: Journal,
    registry: Registry,
    next_cid: u32,
    nonce_counter: u64,
    broker_policy: Option<(
        RegistryPolicy,
        agentkit_sandbox_contracts::ids::PolicyGeneration,
    )>,
}

/// The worker service: shared across all host connections.
pub struct WorkerService {
    config: WorkerConfig,
    inner: Mutex<ServiceInner>,
    backend: Arc<dyn agentkit_sandbox_contracts::api::SandboxBackend>,
    guest_factory: Arc<dyn GuestLinkFactory>,
    links: Mutex<HashMap<SandboxId, Arc<GuestConn>>>,
    pushes: Mutex<Vec<(u64, mpsc::UnboundedSender<WorkerToHost>)>>,
    next_push: Mutex<u64>,
    /// Optional scripted behavior for the in-process fake guest (tests).
    sim_behavior: Mutex<Option<crate::guestlink::SimBehavior>>,
    /// Structured package broker (§9); None = egress impossible in this
    /// process, full stop (deny path never fetches).
    broker: Mutex<Option<Arc<crate::broker::Broker>>>,
    /// Post-restart reconcile runs once, triggered by the first host
    /// `Hello` (§11.4: restart recovery automatic but not optimistic).
    reconcile_pending: std::sync::atomic::AtomicBool,
}

impl WorkerService {
    /// Build the service, replaying the journal into the registry.
    pub fn new(
        config: WorkerConfig,
        backend: Arc<dyn agentkit_sandbox_contracts::api::SandboxBackend>,
        guest_factory: Arc<dyn GuestLinkFactory>,
        ceilings: QuotaCeilings,
    ) -> WorkerResult<Arc<Self>> {
        std::fs::create_dir_all(&config.storage_root)
            .map_err(|e| WorkerError::Storage(format!("storage root: {e}")))?;
        crate::util::set_dir_mode(&config.storage_root, 0o700);
        let journal_dir = config.storage_root.join("journal");
        let journal = Journal::open(&journal_dir, &config.worker_id)?;
        let records = Journal::replay(journal.path())?;
        let registry = Registry::from_journal(&records, ceilings)?;
        Ok(Arc::new(Self {
            config,
            inner: Mutex::new(ServiceInner {
                journal,
                registry,
                next_cid: 3,
                nonce_counter: 0,
                broker_policy: None,
            }),
            backend,
            guest_factory,
            links: Mutex::new(HashMap::new()),
            pushes: Mutex::new(Vec::new()),
            next_push: Mutex::new(0),
            sim_behavior: Mutex::new(None),
            broker: Mutex::new(None),
            reconcile_pending: std::sync::atomic::AtomicBool::new(true),
        }))
    }

    /// Attach the broker (admin startup decision only — S20).
    pub fn attach_broker(&self, broker: Arc<crate::broker::Broker>) {
        *self.broker.lock().expect("broker slot lock") = Some(broker);
    }

    #[must_use]
    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    /// Serialized read access to the authoritative registry.
    pub fn with_registry<T>(&self, f: impl FnOnce(&Registry) -> T) -> T {
        let g = self.inner.lock().expect("service lock");
        f(&g.registry)
    }

    /// Register a host connection for pushed execution events.
    pub fn register_push(&self) -> (u64, mpsc::UnboundedReceiver<WorkerToHost>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut id = self.next_push.lock().expect("push id lock");
        *id += 1;
        self.pushes.lock().expect("pushes lock").push((*id, tx));
        (*id, rx)
    }

    pub fn unregister_push(&self, id: u64) {
        self.pushes
            .lock()
            .expect("pushes lock")
            .retain(|(i, _)| *i != id);
    }

    fn push(&self, msg: WorkerToHost) {
        self.pushes
            .lock()
            .expect("pushes lock")
            .retain(|(_, tx)| tx.send(msg.clone()).is_ok());
    }

    fn journal_append(&self, record: &JournalRecord) -> WorkerResult<u64> {
        let mut g = self.inner.lock().expect("service lock");
        g.journal.append(record, now_ms())
    }

    fn journal_sync(&self) -> WorkerResult<()> {
        let mut g = self.inner.lock().expect("service lock");
        g.journal.sync()
    }

    // -----------------------------------------------------------------------
    // Worker-side path derivation (S20): ONLY from storage root + ids.
    // -----------------------------------------------------------------------

    fn gen_dir(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        generation: InstanceGeneration,
    ) -> PathBuf {
        self.config
            .storage_root
            .join("sandboxes")
            .join(tenant.as_str())
            .join(sandbox.as_str())
            .join(format!("gen-{}", generation.0))
    }

    /// Jailer chroot dir. MUST stay byte-identical to the firecracker
    /// backend's `jailer_id` derivation (same sanitization + format).
    fn jailer_root(&self, sandbox: &SandboxId, generation: InstanceGeneration) -> PathBuf {
        self.config
            .storage_root
            .join("jailer")
            .join(jailer_id(sandbox, generation))
            .join("root")
    }

    fn boot_nonce(&self) -> (Vec<u8>, ArtifactDigest) {
        let mut g = self.inner.lock().expect("service lock");
        g.nonce_counter += 1;
        let seed = format!(
            "{}|{}|{}|{}",
            self.config.worker_id,
            now_ms(),
            g.nonce_counter,
            std::process::id()
        );
        let digest = ArtifactDigest::of_bytes(seed.as_bytes());
        (digest.bytes_hex().as_bytes().to_vec(), digest)
    }

    fn alloc_cid(&self) -> u32 {
        let mut g = self.inner.lock().expect("service lock");
        let cid = g.next_cid;
        g.next_cid += 1;
        cid
    }

    // -----------------------------------------------------------------------
    // Guest link management
    // -----------------------------------------------------------------------

    #[must_use]
    pub fn link(&self, sandbox: &SandboxId) -> Option<Arc<GuestConn>> {
        self.links.lock().expect("links lock").get(sandbox).cloned()
    }

    fn drop_link(&self, sandbox: &SandboxId) {
        if let Some(conn) = self.links.lock().expect("links lock").remove(sandbox) {
            conn.fail_all_fs();
        }
    }

    async fn connect_and_handshake(
        self: &Arc<Self>,
        tenant: &TenantId,
        sandbox: &SandboxId,
        workspace_root: &GuestPath,
    ) -> WorkerResult<Arc<GuestConn>> {
        let (generation, endpoint, nonce, nonce_digest, session_seed) = {
            let mut g = self.inner.lock().expect("service lock");
            let rec = g.registry.sandbox_mut(tenant, sandbox)?;
            (
                rec.generation,
                // The fake transport carries a one-shot stream; take it.
                std::mem::replace(&mut rec.guest_endpoint, GuestEndpoint::replay_placeholder()),
                rec.boot_nonce.clone(),
                rec.boot_nonce_digest.clone(),
                format!("{sandbox}|{}", rec.generation.0),
            )
        };
        let nonce = nonce.ok_or_else(|| {
            WorkerError::GuestLink("boot nonce missing (replayed record; reconcile first)".into())
        })?;
        let link: Box<dyn GuestLink> = self
            .guest_factory
            .connect(sandbox, generation, &endpoint)
            .await?;
        let link = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.do_handshake(link, &nonce, &nonce_digest, workspace_root, &session_seed),
        )
        .await
        .map_err(|_| WorkerError::GuestLink("guest handshake timeout".into()))??;
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToGuest>();
        let conn = Arc::new(GuestConn {
            generation,
            tx,
            pending_fs: Mutex::new(VecDeque::new()),
        });
        let svc = Arc::clone(self);
        let key = sandbox.clone();
        let conn_task = Arc::clone(&conn);
        let mut link = link;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    outgoing = rx.recv() => {
                        match outgoing {
                            Some(msg) => {
                                if let Err(e) = link.send(&msg).await {
                                    tracing_line(&format!("guest send failed for {key}: {e}"));
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    incoming = link.recv() => {
                        match incoming {
                            Ok(msg) => svc.handle_guest_message(&key, msg, &conn_task).await,
                            Err(e) => {
                                tracing_line(&format!("guest link lost for {key}: {e}"));
                                break;
                            }
                        }
                    }
                }
            }
            conn_task.fail_all_fs();
            svc.on_link_lost(&key);
            svc.links.lock().expect("links lock").remove(&key);
        });
        self.links
            .lock()
            .expect("links lock")
            .insert(sandbox.clone(), Arc::clone(&conn));
        // Keep the endpoint on the record for inspect/reconcile.
        {
            let mut g = self.inner.lock().expect("service lock");
            if let Ok(rec) = g.registry.sandbox_mut(tenant, sandbox) {
                rec.guest_endpoint = endpoint;
            }
        }
        Ok(conn)
    }

    async fn do_handshake(
        self: &Arc<Self>,
        mut link: Box<dyn GuestLink>,
        nonce: &[u8],
        nonce_digest: &ArtifactDigest,
        workspace_root: &GuestPath,
        session_seed: &str,
    ) -> WorkerResult<Box<dyn GuestLink>> {
        let hello = WorkerToGuest::HelloAck {
            session_digest: ArtifactDigest::of_bytes(session_seed.as_bytes()),
            protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
            boot_nonce_b64: b64(nonce),
            workspace_root: workspace_root.clone(),
        };
        link.send(&hello).await?;
        let msg = link.recv().await?;
        match msg {
            GuestToWorker::Hello {
                boot_nonce_b64,
                protocol_min,
                protocol_max,
                ..
            } => {
                verify_boot_nonce(&boot_nonce_b64, nonce_digest.as_str())?;
                let want = agentkit_sandbox_contracts::PROTOCOL_VERSION;
                if protocol_min > want || protocol_max < want {
                    return Err(WorkerError::GuestLink(format!(
                        "guest protocol [{protocol_min},{protocol_max}] excludes {want}"
                    )));
                }
                Ok(link)
            }
            other => Err(WorkerError::GuestLink(format!(
                "expected guest hello, got {}",
                guest_op_name(&other)
            ))),
        }
    }

    fn on_link_lost(&self, sandbox: &SandboxId) {
        // Guest loss leaves external effects unknown: executions become
        // Uncertain, the sandbox quarantines for reconcile (S14). We never
        // silently assume commands stopped.
        let tenant = self.with_registry(|reg| reg.sandbox_tenant(sandbox));
        let Some(tenant) = tenant else { return };
        let seq = self
            .journal_append(&JournalRecord::ExecUncertain {
                tenant: tenant.clone(),
                sandbox: sandbox.clone(),
                generation: InstanceGeneration(1),
                execution: ExecutionId::new("link-lost").expect("static id"),
                reason: "guest link lost; in-flight outcomes unknown".into(),
            })
            .unwrap_or(0);
        let mut g = self.inner.lock().expect("service lock");
        g.registry.mark_executions_uncertain_for(&tenant, sandbox);
        let _ = g
            .registry
            .set_quarantined(&tenant, sandbox, "guest link lost", seq);
    }

    async fn handle_guest_message(
        self: &Arc<Self>,
        sandbox: &SandboxId,
        msg: GuestToWorker,
        conn: &Arc<GuestConn>,
    ) {
        match msg {
            GuestToWorker::ExecEvent {
                execution_id,
                sequence,
                kind,
            } => self.on_exec_event(sandbox, execution_id, sequence, kind),
            GuestToWorker::FsResult { outcome, .. } => conn.complete_fs(outcome),
            GuestToWorker::BrokerRequest { request_id, fetch } => {
                // §9: the structured broker is the only egress path. Without
                // an attached (admin-enabled) broker we deny honestly and
                // never touch the network.
                let broker = self.broker.lock().expect("broker slot lock").clone();
                let Some(broker) = broker.filter(|b| b.enabled()) else {
                    let reason = match fetch.validate() {
                        Ok(()) => "broker egress not enabled in this worker config".to_string(),
                        Err(e) => format!("invalid broker request: {}", redact_cap(&e, 120)),
                    };
                    let _ = conn
                        .tx
                        .send(WorkerToGuest::BrokerDeny { request_id, reason });
                    return;
                };
                let (tenant, policy, vm_gen) = {
                    let g = self.inner.lock().expect("service lock");
                    let rec = g.registry.sandbox_by_id(sandbox);
                    match (rec, g.broker_policy.as_ref()) {
                        (Some(r), Some((p, _))) => (r.tenant_id.clone(), p.clone(), r.generation),
                        (None, _) => {
                            let _ = conn.tx.send(WorkerToGuest::BrokerDeny {
                                request_id,
                                reason: "unknown sandbox".into(),
                            });
                            return;
                        }
                        (Some(_), None) => {
                            let _ = conn.tx.send(WorkerToGuest::BrokerDeny {
                                request_id,
                                reason: "no broker policy installed".into(),
                            });
                            return;
                        }
                    }
                };
                if fetch.sandbox_generation != vm_gen.0 {
                    let _ = conn.tx.send(WorkerToGuest::BrokerDeny {
                        request_id,
                        reason: "stale sandbox generation".into(),
                    });
                    return;
                }
                let conn = Arc::clone(conn);
                tokio::spawn(async move {
                    let outcome = broker.fetch(&tenant, &policy, &fetch).await;
                    match outcome {
                        Ok(ok) => {
                            let bytes = match std::fs::read(&ok.path) {
                                Ok(b) => b,
                                Err(_) => {
                                    let _ = conn.tx.send(WorkerToGuest::BrokerDeny {
                                        request_id,
                                        reason: "cache read failed".into(),
                                    });
                                    return;
                                }
                            };
                            let chunks: Vec<&[u8]> = if bytes.is_empty() {
                                Vec::new()
                            } else {
                                bytes
                                    .chunks(agentkit_sandbox_contracts::limits::MAX_CHUNK_BYTES)
                                    .collect()
                            };
                            if chunks.is_empty() {
                                let _ = conn.tx.send(WorkerToGuest::BrokerChunk {
                                    request_id,
                                    sequence: 0,
                                    bytes_b64: String::new(),
                                    chunk_digest: ArtifactDigest::of_bytes(&[]),
                                    final_chunk: true,
                                });
                                return;
                            }
                            for (i, chunk) in chunks.iter().enumerate() {
                                let last = i + 1 == chunks.len();
                                let msg = WorkerToGuest::BrokerChunk {
                                    request_id: request_id.clone(),
                                    sequence: i as u32,
                                    bytes_b64: base64::Engine::encode(
                                        &base64::engine::general_purpose::STANDARD,
                                        chunk,
                                    ),
                                    chunk_digest: ArtifactDigest::of_bytes(chunk),
                                    final_chunk: last,
                                };
                                if conn.tx.send(msg).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(deny) => {
                            let _ = conn.tx.send(WorkerToGuest::BrokerDeny {
                                request_id,
                                reason: redact_cap(&deny.reason, 200),
                            });
                        }
                    }
                });
            }
            GuestToWorker::Health { .. } => {}
            GuestToWorker::Hello { .. } => {
                tracing_line("unexpected duplicate guest hello; ignoring")
            }
        }
    }
    fn on_exec_event(
        self: &Arc<Self>,
        sandbox: &SandboxId,
        execution_id: ExecutionId,
        sequence: u64,
        kind: ExecEventKind,
    ) {
        // Untrusted input: references we cannot resolve are dropped.
        let Some(tenant) = self.with_registry(|reg| reg.execution_tenant(&execution_id)) else {
            tracing_line("exec event for unknown execution; dropping (untrusted)");
            return;
        };
        if self
            .with_registry(|reg| reg.sandbox_tenant(sandbox))
            .is_none()
        {
            tracing_line("exec event for unknown sandbox; dropping (untrusted)");
            return;
        }
        let event = ExecEvent {
            execution_id: execution_id.clone(),
            sequence,
            kind: kind.clone(),
        };
        let generation = self
            .with_registry(|reg| {
                reg.execution(&tenant, &execution_id)
                    .ok()
                    .map(|r| r.generation)
            })
            .unwrap_or(InstanceGeneration(1));
        // Durable report records first (S17: reports, not authority).
        let durable: Option<JournalRecord> = match &kind {
            ExecEventKind::Spawned { guest_pid, .. } => Some(JournalRecord::ExecSpawnedReported {
                tenant: tenant.clone(),
                sandbox: sandbox.clone(),
                generation,
                execution: execution_id.clone(),
                guest_pid: *guest_pid,
            }),
            ExecEventKind::Exited {
                exit_code,
                signal,
                timed_out,
                ..
            } => Some(JournalRecord::ExecExitedReported {
                tenant: tenant.clone(),
                sandbox: sandbox.clone(),
                generation,
                execution: execution_id.clone(),
                code: *exit_code,
                signal: *signal,
                timed_out: *timed_out,
            }),
            _ => None,
        };
        let mut seq = 0;
        if let Some(rec) = &durable {
            seq = self.journal_append(rec).unwrap_or(0);
        }
        let mut g = self.inner.lock().expect("service lock");
        let _ = g
            .registry
            .buffer_event(&tenant, &execution_id, event.clone());
        match &kind {
            ExecEventKind::Spawned { guest_pid, .. } => {
                let _ =
                    g.registry
                        .apply_exec_spawned_reported(&tenant, &execution_id, *guest_pid, seq);
            }
            // `exited` alone never marks CommandState past `Exited` (a
            // report). Worker-side observation continues below.
            ExecEventKind::Exited { .. } => {
                let _ = g
                    .registry
                    .apply_exec_exited_reported(&tenant, &execution_id, seq);
            }
            _ => {}
        }
        drop(g);
        self.push(WorkerToHost::PushExecEvent(event));
        // Worker-side process check: only an independent observation that
        // the backing VM/process is gone yields TerminationObserved.
        if matches!(kind, ExecEventKind::Exited { .. }) {
            self.worker_side_termination_check(&tenant, sandbox, &execution_id, generation);
        }
    }

    fn worker_side_termination_check(
        self: &Arc<Self>,
        tenant: &TenantId,
        sandbox: &SandboxId,
        execution: &ExecutionId,
        generation: InstanceGeneration,
    ) {
        // Backend inspect is the worker-side check (a real hypervisor
        // pid/identity is consulted there; the fake backend mirrors the
        // semantics). Only this observation yields TerminationObserved.
        let handle = self.with_registry(|reg| {
            reg.sandbox(tenant, sandbox).ok().map(|rec| VmHandle {
                sandbox_id: rec.sandbox_id.clone(),
                instance_generation: rec.generation,
                backend: "worker".into(),
                hypervisor_pid: rec.hypervisor_pid,
            })
        });
        let Some(handle) = handle else { return };
        let backend = Arc::clone(&self.backend);
        let h = handle;
        let tenant = tenant.clone();
        let sandbox = sandbox.clone();
        let exec = execution.clone();
        let self_clone = Arc::clone(self);
        tokio::spawn(async move {
            let observed =
                tokio::time::timeout(std::time::Duration::from_secs(5), backend.inspect(&h)).await;
            match observed {
                Ok(Ok(observed)) => {
                    let evidence = format!(
                        "worker-backend-inspect({:?}, pid={:?})",
                        observed.state, observed.hypervisor_pid
                    );
                    let seq = self_clone
                        .journal_append(&JournalRecord::ExecTerminationObserved {
                            tenant: tenant.clone(),
                            sandbox: sandbox.clone(),
                            generation,
                            execution: exec.clone(),
                            evidence,
                        })
                        .unwrap_or(0);
                    let mut g = self_clone.inner.lock().expect("service lock");
                    let _ = g
                        .registry
                        .apply_exec_termination_observed(&tenant, &exec, seq);
                    let seq = g
                        .journal
                        .append(
                            &JournalRecord::ExecCleanupVerified {
                                tenant: tenant.clone(),
                                sandbox: sandbox.clone(),
                                generation,
                                execution: exec.clone(),
                            },
                            now_ms(),
                        )
                        .unwrap_or(seq);
                    let _ = g.registry.apply_exec_cleanup_verified(&tenant, &exec, seq);
                    // Settle Running -> Ready once nothing remains unproven.
                    if !g.registry.has_active_executions(&sandbox)
                        && let Ok(rec) = g.registry.sandbox(&tenant, &sandbox)
                        && rec.state == SandboxState::Running
                    {
                        let _ = g.registry.transition_sandbox(
                            &tenant,
                            &sandbox,
                            SandboxState::Ready,
                            seq,
                        );
                    }
                }
                _ => {
                    // Cannot observe: uncertain, never silently success.
                    let seq = self_clone
                        .journal_append(&JournalRecord::ExecUncertain {
                            tenant: tenant.clone(),
                            sandbox: sandbox.clone(),
                            generation,
                            execution: exec.clone(),
                            reason: "worker-side process check unavailable".into(),
                        })
                        .unwrap_or(0);
                    let mut g = self_clone.inner.lock().expect("service lock");
                    let _ = g.registry.apply_exec_uncertain(&tenant, &exec, seq);
                }
            }
        });
    }

    /// Script the in-process fake guest (fake backend / contract tests).
    pub fn set_sim_behavior(&self, behavior: Option<crate::guestlink::SimBehavior>) {
        *self.sim_behavior.lock().expect("sim behavior lock") = behavior;
    }

    /// Fake backend: hand the registry the worker side of an in-process
    /// duplex pair and spawn the scripted simulator on the guest side.
    /// Firecracker: the jailer vsock UDS (worker-derived path only, S20).
    fn fake_or_vsock_endpoint(
        &self,
        vsock_uds: &std::path::Path,
        cid: u32,
        boot_nonce: &[u8],
    ) -> WorkerResult<GuestEndpoint> {
        if matches!(self.config.backend, crate::backend::BackendKind::Fake) {
            let (worker_side, guest_side) = duplex_pair();
            let behavior = self
                .sim_behavior
                .lock()
                .expect("sim behavior lock")
                .clone()
                .unwrap_or_default();
            let nonce_b64 = b64(boot_nonce);
            tokio::spawn(async move {
                let sim = SimGuest::new(guest_side).with_behavior(behavior);
                let _ = &nonce_b64; // simulator echoes via HelloAck challenge
                sim.serve().await;
            });
            return Ok(GuestEndpoint::fake(worker_side));
        }
        Ok(GuestEndpoint::vsock(
            vsock_uds.to_path_buf(),
            self.config.guest_vsock_port,
            cid,
        ))
    }

    /// Worker-side lifecycle state for one tenant's sandbox (audit/tests).
    pub fn sandbox_state(
        &self,
        tenant: &agentkit_sandbox_contracts::ids::TenantId,
        sandbox: &agentkit_sandbox_contracts::ids::SandboxId,
    ) -> Option<SandboxState> {
        let g = self.inner.lock().expect("service lock");
        g.registry.sandbox(tenant, sandbox).ok().map(|r| r.state)
    }

    /// Durable journal file path (for audit/reconciliation by tests & host).
    #[must_use]
    pub fn journal_path(&self) -> std::path::PathBuf {
        let g = self.inner.lock().expect("service lock");
        g.journal.path().to_path_buf()
    }

    /// Durably flush the journal (used on graceful drain).
    pub fn journal_flush(&self) -> WorkerResult<()> {
        let mut g = self.inner.lock().expect("service lock");
        g.journal.sync()
    }

    // -----------------------------------------------------------------------
    // Dispatch (never panics; every error lands in the reply envelope)
    // -----------------------------------------------------------------------

    /// Handle one decoded request frame. Exactly one reply per request.
    pub async fn handle(self: &Arc<Self>, frame: &WireFrame) -> WorkerToHost {
        let request_id = frame.header.request_id.clone();
        let body: HostToWorker = match serde_json::from_value(frame.body.clone()) {
            Ok(b) => b,
            Err(e) => {
                return WorkerToHost::Reply {
                    request_id,
                    outcome: ReplyOutcomeShape::Err {
                        error: SandboxError::new(
                            SandboxErrorCode::ProtocolIncompatible,
                            format!("malformed request body: {e}"),
                        ),
                    },
                };
            }
        };
        let outcome = match self.dispatch(&request_id, body).await {
            Ok(Direct::Ack(ack)) => return ack,
            Ok(Direct::Body(body)) => ReplyOutcomeShape::Ok { body },
            Err(e) => ReplyOutcomeShape::Err {
                error: e.to_sandbox_error(),
            },
        };
        WorkerToHost::Reply {
            request_id,
            outcome,
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        request_id: &str,
        msg: HostToWorker,
    ) -> WorkerResult<Direct> {
        match msg {
            HostToWorker::Hello {
                host_id,
                protocol_versions,
            } => self.on_hello(&host_id, &protocol_versions).await,
            HostToWorker::CreateVm { envelope, launch } => {
                Ok(Direct::Body(self.on_create_vm(&envelope, &launch).await?))
            }
            HostToWorker::StopVm {
                envelope,
                force,
                reason,
            } => Ok(Direct::Body(
                self.on_stop_vm(&envelope, force, &reason).await?,
            )),
            HostToWorker::DestroyVm { envelope, force } => {
                Ok(Direct::Body(self.on_destroy_vm(&envelope, force).await?))
            }
            HostToWorker::WriterAcquire {
                envelope,
                task_id,
                ttl_ms,
            } => Ok(Direct::Body(
                self.on_writer_acquire(&envelope, &task_id, ttl_ms).await?,
            )),
            HostToWorker::InspectVm { envelope } => {
                Ok(Direct::Body(self.on_inspect_vm(&envelope).await?))
            }
            HostToWorker::ExecStart { grant, request } => {
                Ok(Direct::Body(self.on_exec_start(&grant, &request).await?))
            }
            HostToWorker::ExecCancel {
                grant,
                execution_id,
            } => Ok(Direct::Body(
                self.on_exec_cancel(&grant, &execution_id).await?,
            )),
            HostToWorker::ExecEvents {
                grant,
                execution_id,
                after_sequence,
            } => Ok(Direct::Body(self.on_exec_events(
                &grant,
                &execution_id,
                after_sequence,
            )?)),
            HostToWorker::FsRead {
                grant,
                path,
                from,
                len,
            } => {
                let checked = path.clone();
                Ok(Direct::Body(
                    self.on_fs(
                        request_id,
                        &grant,
                        GuestFsOp::Read { path, from, len },
                        &[checked],
                        SandboxCapability::SandboxRead,
                    )
                    .await?,
                ))
            }
            HostToWorker::FsWrite {
                grant,
                path,
                body_b64,
                expected_sha256,
            } => {
                if base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &body_b64)
                    .map(|b| b.len())
                    .unwrap_or(usize::MAX)
                    > agentkit_sandbox_contracts::limits::MAX_WRITE_BODY_BYTES
                {
                    return Err(WorkerError::Contract(SandboxError::new(
                        SandboxErrorCode::PolicyDenied,
                        "write body exceeds limit",
                    )));
                }
                let checked = path.clone();
                Ok(Direct::Body(
                    self.on_fs(
                        request_id,
                        &grant,
                        GuestFsOp::Write {
                            path,
                            body_b64,
                            expected_sha256,
                            create_parents: true,
                        },
                        &[checked],
                        SandboxCapability::SandboxWrite,
                    )
                    .await?,
                ))
            }
            HostToWorker::FsList {
                grant,
                path,
                options,
            } => {
                let checked = path.clone();
                Ok(Direct::Body(
                    self.on_fs(
                        request_id,
                        &grant,
                        GuestFsOp::List { path, options },
                        &[checked],
                        SandboxCapability::SandboxRead,
                    )
                    .await?,
                ))
            }
            HostToWorker::FsSearch { grant, query } => {
                let root = query.root.clone();
                Ok(Direct::Body(
                    self.on_fs(
                        request_id,
                        &grant,
                        GuestFsOp::Search { query },
                        &[root],
                        SandboxCapability::SandboxRead,
                    )
                    .await?,
                ))
            }
            HostToWorker::FsGitInspect { grant, query } => {
                let repo = query.repo.clone();
                if let Some(rev) = &query.rev
                    && !agentkit_sandbox_contracts::api::validate_git_rev(rev)
                {
                    return Err(WorkerError::Contract(SandboxError::new(
                        SandboxErrorCode::PolicyDenied,
                        "invalid git rev",
                    )));
                }
                Ok(Direct::Body(
                    self.on_fs(
                        request_id,
                        &grant,
                        GuestFsOp::GitInspect { query },
                        &[repo],
                        SandboxCapability::SandboxRead,
                    )
                    .await?,
                ))
            }
            HostToWorker::StageBegin { envelope, manifest } => {
                Ok(Direct::Body(self.on_stage_begin(&envelope, &manifest)?))
            }
            HostToWorker::StageChunk {
                envelope,
                stage_id,
                sequence,
                bytes_b64,
                chunk_digest,
            } => Ok(Direct::Body(self.on_stage_chunk(
                &envelope,
                &stage_id,
                sequence,
                &bytes_b64,
                &chunk_digest,
            )?)),
            HostToWorker::StageCommit {
                envelope,
                stage_id,
                manifest_digest,
            } => Ok(Direct::Body(self.on_stage_commit(
                &envelope,
                &stage_id,
                &manifest_digest,
            )?)),
            HostToWorker::StageAbort { envelope, stage_id } => {
                Ok(Direct::Body(self.on_stage_abort(&envelope, &stage_id)?))
            }
            HostToWorker::ImportCommit {
                grant,
                stage_id,
                manifest_digest,
            } => Ok(Direct::Body(
                self.on_import_commit(&grant, &stage_id, &manifest_digest)
                    .await?,
            )),
            HostToWorker::ExportRequest {
                grant,
                stage_id,
                paths,
            } => Ok(Direct::Body(
                self.on_export_request(&grant, &stage_id, &paths).await?,
            )),
            HostToWorker::BrokerPolicyPut { policy, generation } => Ok(Direct::Body(
                self.on_broker_policy_put(&policy, generation)?,
            )),
            HostToWorker::ReconcileQuery {} => Ok(Direct::Body(self.on_reconcile_query()?)),
            HostToWorker::GetCapabilities {} => Ok(Direct::Body(self.on_get_capabilities().await?)),
        }
    }

    async fn on_hello(
        self: &Arc<Self>,
        host_id: &str,
        protocol_versions: &[u32],
    ) -> WorkerResult<Direct> {
        // First authenticated host contact after a (re)start: run the
        // journal-replayed reconcile pass in the background. Until it
        // proves each replayed VM's physical state, those sandboxes stay
        // quarantined -- operations refuse, they never guess (S14).
        if self
            .reconcile_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let svc = Arc::clone(self);
            tokio::spawn(async move {
                svc.reconcile_replayed().await;
            });
        }
        let want = agentkit_sandbox_contracts::PROTOCOL_VERSION;
        if !protocol_versions.contains(&want) {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ProtocolIncompatible,
                format!("host protocols {protocol_versions:?} exclude worker protocol {want}"),
            )));
        }
        let capabilities = self.backend.probe().await?;
        let identity = WorkerIdentity {
            worker_id: WorkerId::new(&self.config.worker_id)
                .map_err(|e| WorkerError::Internal(format!("worker id: {e}")))?,
            revision: self.config.revision,
            install_fingerprint: agentkit_sandbox_contracts::ids::InstallFingerprint::parse(
                &self.config.install_fingerprint,
            )
            .map_err(|e| WorkerError::Internal(format!("fingerprint: {e}")))?,
        };
        tracing_line(&format!("hello from host {host_id}"));
        Ok(Direct::Ack(WorkerToHost::HelloAck {
            identity,
            protocol_version: want,
            capabilities,
        }))
    }

    async fn on_get_capabilities(&self) -> WorkerResult<WorkerReplyBody> {
        Ok(WorkerReplyBody::Capabilities {
            capabilities: self.backend.probe().await?,
        })
    }

    fn on_reconcile_query(&self) -> WorkerResult<WorkerReplyBody> {
        let worker_id = WorkerId::new(&self.config.worker_id)
            .map_err(|e| WorkerError::Internal(format!("worker id: {e}")))?;
        let mut inventory = self.with_registry(|reg| reg.inventory(&worker_id));
        inventory.worker_revision = self.config.revision;
        Ok(WorkerReplyBody::Inventory { inventory })
    }

    fn on_broker_policy_put(
        &self,
        policy: &RegistryPolicy,
        generation: agentkit_sandbox_contracts::ids::PolicyGeneration,
    ) -> WorkerResult<WorkerReplyBody> {
        policy
            .validate()
            .map_err(|e| WorkerError::Protocol(format!("broker policy: {e}")))?;
        let mut g = self.inner.lock().expect("service lock");
        g.broker_policy = Some((policy.clone(), generation));
        Ok(WorkerReplyBody::Ack {})
    }

    // -----------------------------------------------------------------------
    // VM lifecycle
    // -----------------------------------------------------------------------

    fn check_envelope(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        required: SandboxCapability,
    ) -> WorkerResult<()> {
        if envelope.capability != required {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::PolicyDenied,
                    "control envelope capability mismatch",
                )
                .with_detail(format!("{:?}", envelope.capability)),
            ));
        }
        Ok(())
    }

    /// The envelope pins the authoritative instance generation (S01): a
    /// control request naming a stale generation is refused, never acted
    /// on. Registry lookup failure (unknown/foreign sandbox) propagates
    /// first with its non-disclosing error.
    fn check_generation(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        generation: InstanceGeneration,
    ) -> WorkerResult<()> {
        let current =
            self.with_registry(|reg| reg.sandbox(tenant, sandbox).map(|r| r.generation))?;
        if current != generation {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::StaleGeneration,
                    "control envelope targets a stale instance generation",
                )
                .with_sandbox(sandbox, Some(current.0)),
            ));
        }
        Ok(())
    }

    /// Post-restart physical reconciliation (§11.4, T22; S14). For every
    /// sandbox the journal replay left quarantined, the worker must
    /// INDEPENDENTLY prove what the hypervisor process is doing now:
    ///
    /// * already terminated (backend has no live VM)  -> durable `VmStopped`
    ///   evidence, quarantine released, sandbox `Stopped`;
    /// * still running -> its guest session died with the old worker
    ///   (boot-nonce channel is one-shot, §7), so command/process
    ///   authorization may never be reused: terminate with evidence and
    ///   quarantine only if that termination is unproven;
    /// * backend unreachable -> stay quarantined, refuse everything.
    ///
    /// Nothing is adopted optimistically; a reboot is always a NEW
    /// instance_generation.
    pub async fn reconcile_replayed(self: &Arc<Self>) {
        let victims = self.with_registry(|reg| reg.reconcile_pending_list());
        if victims.is_empty() {
            return;
        }
        tracing_line(&format!(
            "reconcile: {} replayed sandbox(es) to verify",
            victims.len()
        ));
        for (tenant, sandbox) in victims {
            let Ok(handle) = self.handle_for(&tenant, &sandbox) else {
                continue;
            };
            let evidence = match self.backend.inspect(&handle).await {
                Ok(obs) if matches!(obs.state, VmPhysState::Stopped | VmPhysState::NotCreated) => {
                    Some(TerminationEvidence {
                        method: "reconcile:already-terminated".into(),
                        observed_ms: now_ms(),
                        identity: obs.process_identity,
                        conclusive: true,
                    })
                }
                Ok(_running) => {
                    // Alive but unaddressable (nonce lost with the old
                    // worker): reclaim the process; never resume authority.
                    tracing_line(&format!(
                        "reconcile: {tenant}/{sandbox} survivor, terminating"
                    ));
                    self.backend.terminate(&handle).await.ok()
                }
                Err(_) => None,
            };
            let Some(evidence) = evidence else {
                // Cannot prove physical state: quarantine persists.
                tracing_line(&format!(
                    "reconcile: {tenant}/{sandbox} stays quarantined (state unproven)"
                ));
                continue;
            };
            let conclusive = evidence.conclusive;
            let Ok(seq) = self.journal_append(&JournalRecord::VmStopped {
                tenant: tenant.clone(),
                sandbox: sandbox.clone(),
                generation: handle.instance_generation,
                evidence: evidence.clone(),
            }) else {
                // No durable record => no release.
                continue;
            };
            let mut g = self.inner.lock().expect("service lock");
            let _ = g
                .registry
                .apply_vm_stopped(&tenant, &sandbox, &evidence, now_ms(), seq);
            if conclusive {
                let _ = g.registry.clear_quarantine(&tenant, &sandbox, seq);
            } else {
                let _ = g.registry.set_quarantined(
                    &tenant,
                    &sandbox,
                    "reconcile: termination unverified",
                    seq,
                );
            }
        }
    }

    async fn on_create_vm(
        self: &Arc<Self>,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        launch: &VerifiedLaunchSpec,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        // Envelope vs launch: one authoritative binding (S02).
        if envelope.tenant_id != launch.tenant_id
            || envelope.sandbox_id != launch.sandbox_id
            || envelope.instance_generation != launch.instance_generation
            || envelope.policy_digest != launch.spec.policy_digest
            || envelope.policy_generation != launch.spec.policy_generation
        {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "control envelope does not match launch spec binding",
            )));
        }
        let tenant = &launch.tenant_id;
        let sandbox = &launch.sandbox_id;
        let generation = launch.instance_generation;

        // Idempotent replay of an already-created generation.
        if let Ok(rec) = self.with_registry(|reg| {
            reg.sandbox(tenant, sandbox)
                .map(|r| (r.generation, r.hypervisor_pid, r.physical_state))
        }) {
            let (existing_gen, pid, physical) = rec;
            if existing_gen != generation {
                // Re-launch of a known sandbox identity: exactly ONE
                // generation ahead, and only when the prior generation is
                // PROVEN terminated (S01/§8.3: every boot is a new
                // generation; quarantined/unknown never reboots in place).
                if !(generation.0 == existing_gen.0 + 1 && matches!(physical, VmPhysState::Stopped))
                {
                    return Err(WorkerError::Contract(
                        SandboxError::new(SandboxErrorCode::StaleGeneration, "generation mismatch")
                            .with_sandbox(sandbox, Some(generation.0)),
                    ));
                }
                // Fall through to the full create path for the new boot.
            } else {
                let observed = VmObservedState {
                    state: physical,
                    hypervisor_pid: pid,
                    process_identity: self.with_registry(|reg| {
                        reg.sandbox(tenant, sandbox)
                            .ok()
                            .and_then(|r| r.identity.clone())
                    }),
                    last_observed_ms: now_ms(),
                };
                return Ok(WorkerReplyBody::Created {
                    vm: VmHandle {
                        sandbox_id: sandbox.clone(),
                        instance_generation: generation,
                        backend: self.config.backend_flag(),
                        hypervisor_pid: pid,
                    },
                    observed,
                });
            }
        }

        // Quota reservation BEFORE any effect (over ceiling => refused, no
        // disk, no launch).
        let vcpus = launch.spec.resources.vcpus;
        let memory_mib = launch.spec.resources.memory_mib;
        let disk_mib = launch.spec.resources.writable_disk_mib as u64;
        {
            let mut g = self.inner.lock().expect("service lock");
            g.registry
                .reserve(tenant, vcpus, memory_mib, disk_mib)
                .map_err(WorkerError::Contract)?;
        }
        let _seq = self.journal_append(&JournalRecord::ReservationAcquire {
            tenant: tenant.clone(),
            sandbox: Some(sandbox.clone()),
            generation: Some(generation),
            vcpus,
            memory_mib,
            disk_mib,
        })?;

        // Worker-derived paths only (S20).
        let gen_dir = self.gen_dir(tenant, sandbox, generation);
        let data_raw = gen_dir.join("data.raw");
        let disk_lock = gen_dir.join("data.raw.lock");
        let pidfile = gen_dir.join("fc.pid");
        let jailer_root = self.jailer_root(sandbox, generation);
        let api_uds = jailer_root.join("run/firecracker.socket");
        let vsock_uds = jailer_root.join("vsock.uds");
        let _ = api_uds; // api socket owned by the backend/jailer crate
        // The host mints the boot nonce at admission; the worker binds it
        // against its own digest (channel association, §3.3). A launch
        // whose nonce does not hash to the recorded digest is refused.
        let boot_nonce = crate::guestlink::unb64(&launch.boot_nonce_b64)?;
        let boot_nonce_digest = ArtifactDigest::of_bytes(&boot_nonce);
        if boot_nonce_digest != launch.boot_nonce_digest {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ProtocolIncompatible,
                "boot nonce does not hash to the launch's recorded digest",
            )));
        }
        let _ = self.boot_nonce(); // keep the local nonce counter advancing (audit entropy)
        let cid = self.alloc_cid();

        let seq = self.journal_append(&JournalRecord::VmCreated {
            tenant: tenant.clone(),
            sandbox: sandbox.clone(),
            generation,
            nonce_digest: boot_nonce_digest.clone(),
            vsock_cid: cid,
            disk_path: data_raw.to_string_lossy().to_string(),
            disk_digest: None,
            image_digest: launch.image_digest.clone(),
            rootfs_digest: launch.rootfs_digest.clone(),
            kernel_digest: launch.trusted_kernel_digest.clone(),
            policy_digest: launch.spec.policy_digest.clone(),
            policy_generation: launch.spec.policy_generation,
            allow_guest_root: launch.allow_guest_root,
            persistent: matches!(
                launch.spec.lifecycle,
                agentkit_sandbox_contracts::spec::Lifecycle::Persistent
            ),
        })?;

        // Prepare private disk (effects after durable VmCreated).
        std::fs::create_dir_all(&gen_dir)
            .map_err(|e| WorkerError::Storage(format!("generation dir: {e}")))?;
        crate::util::set_dir_mode(&gen_dir, 0o700);
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&data_raw)
            .map_err(|e| WorkerError::Storage(format!("disk create: {e}")))?;
        drop(file);
        let disk_digest = ArtifactDigest::of_bytes(&[0u8; 0]);
        self.journal_append(&JournalRecord::DiskCreated {
            tenant: tenant.clone(),
            sandbox: sandbox.clone(),
            generation,
            disk_path: data_raw.to_string_lossy().to_string(),
            disk_digest: disk_digest.clone(),
        })?;

        {
            let mut g = self.inner.lock().expect("service lock");
            g.registry
                .apply_vm_created(
                    NewVmSpec {
                        tenant_id: tenant.clone(),
                        sandbox_id: sandbox.clone(),
                        generation,
                        image_digest: launch.image_digest.clone(),
                        rootfs_digest: launch.rootfs_digest.clone(),
                        kernel_digest: launch.trusted_kernel_digest.clone(),
                        policy_digest: launch.spec.policy_digest.clone(),
                        policy_generation: launch.spec.policy_generation,
                        boot_nonce_digest: boot_nonce_digest.clone(),
                        vsock_cid: cid,
                        allow_guest_root: launch.allow_guest_root,
                        gen_dir: gen_dir.clone(),
                        data_raw: data_raw.clone(),
                        disk_lock: disk_lock.clone(),
                        pidfile: pidfile.clone(),
                        guest_endpoint: self.fake_or_vsock_endpoint(
                            &vsock_uds,
                            cid,
                            &boot_nonce,
                        )?,
                        persistent: matches!(
                            launch.spec.lifecycle,
                            agentkit_sandbox_contracts::spec::Lifecycle::Persistent
                        ),
                        last_evidence: None,
                        reserved_vcpus: vcpus,
                        reserved_memory_mib: memory_mib,
                        reserved_disk_mib: disk_mib,
                        now_ms: now_ms(),
                    },
                    seq,
                )
                .inspect_err(|_| {
                    self.rollback_reservation(tenant, vcpus, memory_mib, disk_mib);
                })?;
            if let Ok(rec) = g.registry.sandbox_mut(tenant, sandbox) {
                rec.boot_nonce = Some(boot_nonce.clone());
            }
        }

        // Provisioning: Requested -> Provisioning -> (launch|Failed).
        self.transition(tenant, sandbox, SandboxState::Provisioning)?;
        let outcome = self
            .provision_launch_handshake(tenant, sandbox, launch)
            .await;
        match outcome {
            Ok((handle, observed)) => {
                self.transition(tenant, sandbox, SandboxState::Ready)?;
                Ok(WorkerReplyBody::Created {
                    vm: handle,
                    observed,
                })
            }
            Err(e) => {
                // Failed -> quarantine of resources; cleanup through the
                // normal destroy path with reservations held (truthful).
                let _ = self.transition(tenant, sandbox, SandboxState::Failed);
                self.drop_link(sandbox);
                let mut g = self.inner.lock().expect("service lock");
                let _ = g
                    .registry
                    .set_quarantined(tenant, sandbox, "create failed", seq);
                drop(g);
                Err(e)
            }
        }
    }

    async fn provision_launch_handshake(
        self: &Arc<Self>,
        tenant: &TenantId,
        sandbox: &SandboxId,
        launch: &VerifiedLaunchSpec,
    ) -> WorkerResult<(VmHandle, VmObservedState)> {
        let handle = self.backend.provision(launch).await?;
        let observed = self.backend.launch(&handle).await?;
        let seq = self.journal_append(&JournalRecord::VmLaunched {
            tenant: tenant.clone(),
            sandbox: sandbox.clone(),
            generation: launch.instance_generation,
            pid: observed.hypervisor_pid,
            identity: observed.process_identity.clone(),
        })?;
        {
            let mut g = self.inner.lock().expect("service lock");
            g.registry.apply_vm_launched(
                tenant,
                sandbox,
                observed.hypervisor_pid,
                observed.process_identity.clone(),
                observed.last_observed_ms.max(now_ms()),
                seq,
            )?;
        }
        self.connect_and_handshake(tenant, sandbox, &launch.spec.workspace_root)
            .await?;
        Ok((handle, observed))
    }

    fn transition(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        to: SandboxState,
    ) -> WorkerResult<()> {
        let mut g = self.inner.lock().expect("service lock");
        let seq = g.journal.next_seq();
        g.registry.transition_sandbox(tenant, sandbox, to, seq)
    }

    fn rollback_reservation(&self, tenant: &TenantId, vcpus: u32, memory_mib: u32, disk_mib: u64) {
        let mut g = self.inner.lock().expect("service lock");
        if g.registry
            .release(tenant, vcpus, memory_mib, disk_mib)
            .is_ok()
        {
            let _ = g.journal.append(
                &JournalRecord::ReservationRelease {
                    tenant: tenant.clone(),
                    sandbox: None,
                    generation: None,
                    vcpus,
                    memory_mib,
                    disk_mib,
                },
                now_ms(),
            );
        }
    }

    fn handle_for(&self, tenant: &TenantId, sandbox: &SandboxId) -> WorkerResult<VmHandle> {
        self.with_registry(|reg| {
            reg.sandbox(tenant, sandbox).map(|rec| VmHandle {
                sandbox_id: rec.sandbox_id.clone(),
                instance_generation: rec.generation,
                backend: self.config.backend_flag(),
                hypervisor_pid: rec.hypervisor_pid,
            })
        })
    }

    async fn on_stop_vm(
        self: &Arc<Self>,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        force: bool,
        reason: &str,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        let tenant = &envelope.tenant_id;
        let sandbox = &envelope.sandbox_id;
        self.check_generation(tenant, sandbox, envelope.instance_generation)?;
        let state = self.with_registry(|reg| reg.sandbox(tenant, sandbox).map(|r| r.state))?;
        if state == SandboxState::Stopped {
            let evidence = self
                .with_registry(|reg| {
                    reg.sandbox(tenant, sandbox)
                        .ok()
                        .and_then(|r| r.last_evidence.clone())
                })
                .unwrap_or(TerminationEvidence {
                    method: "journal".into(),
                    observed_ms: now_ms(),
                    identity: None,
                    conclusive: true,
                });
            return Ok(WorkerReplyBody::Stopped { evidence });
        }
        self.check_stoppable(state, sandbox)?;
        // Stopping is entered from Ready/Running (table-authoritative).
        if matches!(state, SandboxState::Ready | SandboxState::Running) {
            self.transition(tenant, sandbox, SandboxState::Stopping)?;
        }
        // Ask the guest to power off (bounded, best effort), then cut.
        if let Some(conn) = self.link(sandbox) {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                self.send_guest(sandbox, &conn, WorkerToGuest::Shutdown { force }),
            )
            .await;
        }
        self.drop_link(sandbox);
        let handle = self.handle_for(tenant, sandbox)?;
        let evidence = self.backend.terminate(&handle).await?;
        let seq = self.journal_append(&JournalRecord::VmStopped {
            tenant: tenant.clone(),
            sandbox: sandbox.clone(),
            generation: envelope.instance_generation,
            evidence: evidence.clone(),
        })?;
        {
            let mut g = self.inner.lock().expect("service lock");
            g.registry
                .apply_vm_stopped(tenant, sandbox, &evidence, now_ms(), seq)?;
            g.registry.mark_executions_uncertain_for(tenant, sandbox);
        }
        if !evidence.conclusive {
            // No proven termination: quarantine + honest uncertainty.
            if matches!(state, SandboxState::Ready | SandboxState::Running) {
                let _ = self.transition(tenant, sandbox, SandboxState::Quarantined);
            }
            let mut g = self.inner.lock().expect("service lock");
            let _ = g.registry.set_quarantined(
                tenant,
                sandbox,
                &format!("stop unverified: {reason}"),
                seq,
            );
            // Partial truth on the success path: the evidence itself says
            // `conclusive: false`; the sandbox now reads Quarantined.
            return Ok(WorkerReplyBody::Stopped { evidence });
        }
        let _ = self.transition(tenant, sandbox, SandboxState::Stopped);
        Ok(WorkerReplyBody::Stopped { evidence })
    }

    fn check_stoppable(&self, state: SandboxState, sandbox: &SandboxId) -> WorkerResult<()> {
        match state {
            SandboxState::Ready
            | SandboxState::Running
            | SandboxState::Stopping
            | SandboxState::Stopped
            | SandboxState::Quarantined
            | SandboxState::Failed => Ok(()),
            other => Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::PolicyDenied,
                    format!("cannot stop a sandbox in state {other:?}"),
                )
                .with_sandbox(sandbox, None),
            )),
        }
    }

    async fn on_destroy_vm(
        self: &Arc<Self>,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        force: bool,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        let tenant = &envelope.tenant_id;
        let sandbox = &envelope.sandbox_id;
        self.check_generation(tenant, sandbox, envelope.instance_generation)?;
        let rec = self.with_registry(|reg| {
            reg.sandbox(tenant, sandbox).map(|r| {
                (
                    r.state,
                    r.physical_state,
                    r.generation,
                    r.data_raw.clone(),
                    r.hypervisor_pid,
                    r.reserved_vcpus,
                    r.reserved_memory_mib,
                    r.reserved_disk_mib,
                )
            })
        })?;
        let (state, physical, generation, data_raw, pid, rv, rm, rd) = rec;
        if matches!(state, SandboxState::Destroyed) {
            return Err(self.with_registry(|reg| reg.sandbox(tenant, sandbox).unwrap_err()));
        }
        // Terminate first unless proven stopped.
        let mut evidence = self.with_registry(|reg| {
            reg.sandbox(tenant, sandbox)
                .ok()
                .and_then(|r| r.last_evidence.clone())
        });
        if !matches!(physical, VmPhysState::Stopped) {
            if let Some(conn) = self.link(sandbox) {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    self.send_guest(sandbox, &conn, WorkerToGuest::Shutdown { force }),
                )
                .await;
            }
            self.drop_link(sandbox);
            let handle = self.handle_for(tenant, sandbox)?;
            evidence = Some(self.backend.terminate(&handle).await?);
            if let Some(ev) = &evidence {
                let seq = self.journal_append(&JournalRecord::VmStopped {
                    tenant: tenant.clone(),
                    sandbox: sandbox.clone(),
                    generation,
                    evidence: ev.clone(),
                })?;
                let mut g = self.inner.lock().expect("service lock");
                let _ = g
                    .registry
                    .apply_vm_stopped(tenant, sandbox, ev, now_ms(), seq);
            }
        }
        let conclusive = evidence
            .as_ref()
            .is_some_and(|e| e.conclusive || matches!(physical, VmPhysState::Stopped));
        if !conclusive {
            // Partial truth: retain everything, quarantine, non-empty retained.
            if matches!(state, SandboxState::Ready | SandboxState::Running) {
                let _ = self.transition(tenant, sandbox, SandboxState::Stopping);
                let _ = self.transition(tenant, sandbox, SandboxState::Quarantined);
            }
            let mut g = self.inner.lock().expect("service lock");
            let seq = g.journal.next_seq();
            let _ =
                g.registry
                    .set_quarantined(tenant, sandbox, "destroy: termination unverified", seq);
            let mut retained = vec![format!("disk:{}", data_raw.to_string_lossy())];
            if let Some(p) = pid {
                retained.push(format!("hypervisor-pid:{p}"));
            }
            return Ok(WorkerReplyBody::Destroyed {
                receipt: DeletionReceipt {
                    sandbox_id: sandbox.clone(),
                    instance_generation: generation,
                    storage_deleted: false,
                    reservations_released: false,
                    termination: evidence.unwrap_or(TerminationEvidence {
                        method: "none".into(),
                        observed_ms: now_ms(),
                        identity: None,
                        conclusive: false,
                    }),
                    retained,
                },
            });
        }
        // Verified path: Destroying -> destroyed effects -> Destroyed.
        match state {
            SandboxState::Ready
            | SandboxState::Stopped
            | SandboxState::Quarantined
            | SandboxState::Failed => {
                self.transition(tenant, sandbox, SandboxState::Destroying)?;
            }
            SandboxState::Stopping => {
                self.transition(tenant, sandbox, SandboxState::Stopped)?;
                self.transition(tenant, sandbox, SandboxState::Destroying)?;
            }
            other => {
                return Err(WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::PolicyDenied,
                    format!("cannot destroy a sandbox in state {other:?}"),
                )));
            }
        }
        let ev = evidence.clone().unwrap_or(TerminationEvidence {
            method: "journal".into(),
            observed_ms: now_ms(),
            identity: None,
            conclusive: true,
        });
        self.journal_append(&JournalRecord::VmDestroyed {
            tenant: tenant.clone(),
            sandbox: sandbox.clone(),
            generation,
            evidence: ev.clone(),
        })?;
        let mut storage_deleted = true;
        match std::fs::remove_dir_all(data_raw.parent().unwrap_or(&data_raw)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                storage_deleted = false;
                tracing_line(&format!("destroy: storage delete failed: {e}"));
            }
        }
        self.journal_append(&JournalRecord::DiskDeleted {
            tenant: tenant.clone(),
            sandbox: sandbox.clone(),
            generation,
            disk_path: data_raw.to_string_lossy().to_string(),
        })?;
        let mut reservations_released = true;
        {
            let mut g = self.inner.lock().expect("service lock");
            if g.registry.release(tenant, rv, rm, rd).is_ok() {
                let _ = g.journal.append(
                    &JournalRecord::ReservationRelease {
                        tenant: tenant.clone(),
                        sandbox: Some(sandbox.clone()),
                        generation: Some(generation),
                        vcpus: rv,
                        memory_mib: rm,
                        disk_mib: rd,
                    },
                    now_ms(),
                );
            } else {
                reservations_released = false;
            }
        }
        let mut retained = Vec::new();
        if !storage_deleted {
            retained.push(format!("disk:{}", data_raw.to_string_lossy()));
        }
        if !reservations_released {
            retained.push("reservations".into());
        }
        {
            let mut g = self.inner.lock().expect("service lock");
            let seq = g.journal.next_seq();
            if storage_deleted && reservations_released {
                let _ =
                    g.registry
                        .transition_sandbox(tenant, sandbox, SandboxState::Destroyed, seq);
                let _ = g.registry.apply_vm_destroyed(tenant, sandbox);
            }
        }
        self.drop_link(sandbox);
        Ok(WorkerReplyBody::Destroyed {
            receipt: DeletionReceipt {
                sandbox_id: sandbox.clone(),
                instance_generation: generation,
                storage_deleted,
                reservations_released,
                termination: ev,
                retained,
            },
        })
    }

    async fn on_inspect_vm(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        let tenant = &envelope.tenant_id;
        let sandbox = &envelope.sandbox_id;
        self.check_generation(tenant, sandbox, envelope.instance_generation)?;
        let (physical, identity) = self.with_registry(|reg| {
            reg.sandbox(tenant, sandbox)
                .map(|r| (r.physical_state, r.identity.clone()))
        })?;
        let observed = match self.handle_for(tenant, sandbox) {
            Ok(handle) => self
                .backend
                .inspect(&handle)
                .await
                .unwrap_or(VmObservedState {
                    state: physical,
                    hypervisor_pid: None,
                    process_identity: identity.clone(),
                    last_observed_ms: now_ms(),
                }),
            Err(_) => VmObservedState {
                state: physical,
                hypervisor_pid: None,
                process_identity: identity.clone(),
                last_observed_ms: now_ms(),
            },
        };
        Ok(WorkerReplyBody::Inspected { observed, identity })
    }

    // -----------------------------------------------------------------------
    // Executions
    // -----------------------------------------------------------------------

    fn verify_grant(
        &self,
        tenant_lookup: &TenantId,
        sandbox_id: &SandboxId,
        grant: &WorkspaceGrant,
        capability: SandboxCapability,
        request_digest: &RequestDigest,
    ) -> WorkerResult<(
        InstanceGeneration,
        agentkit_sandbox_contracts::ids::PolicyDigest,
        agentkit_sandbox_contracts::ids::PolicyGeneration,
    )> {
        let (generation, policy_digest, policy_generation, state) = self.with_registry(|reg| {
            reg.sandbox(tenant_lookup, sandbox_id).map(|r| {
                (
                    r.generation,
                    r.policy_digest.clone(),
                    r.policy_generation,
                    r.state,
                )
            })
        })?;
        if state.denies_all() {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::Quarantined,
                    "sandbox denies all operations until reconciliation",
                )
                .with_sandbox(sandbox_id, Some(generation.0)),
            ));
        }
        if !state.allows_workspace_io() {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::Unavailable,
                    "sandbox is not accepting workspace operations",
                )
                .with_sandbox(sandbox_id, Some(generation.0)),
            ));
        }
        grant
            .verify(&GrantExpectation {
                tenant: grant.tenant_id.clone(),
                sandbox: sandbox_id.clone(),
                generation,
                policy_generation,
                policy_digest: policy_digest.clone(),
                capability,
                request_digest: request_digest.clone(),
                now_ms: now_ms(),
            })
            .map_err(WorkerError::Contract)?;
        // Writer gate (S09): mutating capabilities additionally require a
        // live, current-epoch lease admitted by THIS worker (durable
        // through the WriterAcquired journal record).
        self.with_registry(|reg| {
            reg.verify_writer(tenant_lookup, sandbox_id, capability, grant, now_ms())
        })?;
        Ok((generation, policy_digest, policy_generation))
    }

    /// Admit the next single writer (S09/T16). Journal-before-effect: the
    /// `WriterAcquired` record is durable under the same service lock as
    /// the registry bump, so a crash either admits nothing (no record:
    /// replay keeps the old epoch, and a retry re-admits) or admits
    /// exactly this writer.
    async fn on_writer_acquire(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        task_id: &agentkit_sandbox_contracts::ids::TaskId,
        ttl_ms: u64,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        let tenant = envelope.tenant_id.clone();
        let sandbox = envelope.sandbox_id.clone();
        let generation = envelope.instance_generation;
        let mut g = self.inner.lock().expect("service lock");
        // Validate BEFORE the durable append: no journal record for a
        // request we would refuse.
        {
            let rec = g.registry.sandbox(&tenant, &sandbox)?;
            if rec.generation != generation {
                return Err(WorkerError::Contract(
                    SandboxError::new(
                        SandboxErrorCode::StaleGeneration,
                        "writer admission targets a stale instance generation",
                    )
                    .with_sandbox(&sandbox, Some(rec.generation.0)),
                ));
            }
            if rec.state.denies_all() {
                return Err(WorkerError::Contract(
                    SandboxError::new(
                        SandboxErrorCode::Quarantined,
                        "sandbox denies all operations until reconciliation",
                    )
                    .with_sandbox(&sandbox, Some(generation.0)),
                ));
            }
            if !rec.state.allows_workspace_io() {
                return Err(WorkerError::Contract(
                    SandboxError::new(
                        SandboxErrorCode::Unavailable,
                        "sandbox is not accepting workspace operations",
                    )
                    .with_sandbox(&sandbox, Some(generation.0)),
                ));
            }
        }
        let next_epoch = g.registry.sandbox(&tenant, &sandbox)?.writer_epoch + 1;
        let now = now_ms();
        let seq = g.journal.append(
            &JournalRecord::WriterAcquired {
                tenant: tenant.clone(),
                sandbox: sandbox.clone(),
                generation,
                epoch: next_epoch,
                holder_task: task_id.clone(),
                expires_at_ms: now.saturating_add(ttl_ms),
            },
            now,
        )?;
        g.journal.sync()?;
        let token = g
            .registry
            .acquire_writer(&tenant, &sandbox, generation, task_id, ttl_ms, now, seq)?;
        Ok(WorkerReplyBody::WriterLease { token })
    }

    async fn on_exec_start(
        self: &Arc<Self>,
        grant: &WorkspaceGrant,
        request: &agentkit_sandbox_contracts::api::ExecRequest,
    ) -> WorkerResult<WorkerReplyBody> {
        let request_digest = digest_of_request(request)?;
        if request.request_digest != request_digest {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ProtocolIncompatible,
                "request_digest does not match the canonical request body",
            )));
        }
        let (generation, _, _) = self.verify_grant(
            &grant.tenant_id,
            &grant.sandbox_id,
            grant,
            SandboxCapability::ProcessExec,
            &request_digest,
        )?;
        // Idempotency: duplicate execution_id + identical digest replays the
        // recorded result; conflicting digest is a protocol error (S07).
        if let Some(existing) = self.with_registry(|reg| {
            reg.exec_exists(&grant.tenant_id, &request.execution_id)
                .map(|r| (r.request_digest.clone(), r.state))
        }) {
            let (recorded, state) = existing;
            if recorded == request_digest {
                return Ok(WorkerReplyBody::ExecStarted {
                    handle: ExecutionHandle {
                        execution_id: request.execution_id.clone(),
                        state,
                        replayed: true,
                    },
                });
            }
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::ProtocolIncompatible,
                    "execution_id reused with a conflicting digest",
                )
                .with_sandbox(&grant.sandbox_id, Some(generation.0)),
            ));
        }
        if self.link(&grant.sandbox_id).is_none() {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ExecutionNotStarted,
                "no guest association for this generation",
            )));
        }
        let grant_digest = RequestDigest::of_canonical_json(&canonical_body(grant)?);
        // Admitted -> Dispatched -> Acked, each durable before the next.
        let seq = self.journal_append(&JournalRecord::ExecAdmitted {
            tenant: grant.tenant_id.clone(),
            sandbox: grant.sandbox_id.clone(),
            generation,
            execution: request.execution_id.clone(),
            grant_digest: grant_digest.clone(),
        })?;
        {
            let mut g = self.inner.lock().expect("service lock");
            g.registry.apply_exec_admitted(
                grant.tenant_id.clone(),
                grant.sandbox_id.clone(),
                generation,
                request.execution_id.clone(),
                request_digest.clone(),
                grant_digest,
                seq,
            )?;
            if let Ok(rec) = g.registry.sandbox(&grant.tenant_id, &grant.sandbox_id)
                && rec.state == SandboxState::Ready
            {
                let _ = g.registry.transition_sandbox(
                    &grant.tenant_id,
                    &grant.sandbox_id,
                    SandboxState::Running,
                    seq,
                );
            }
        }
        self.journal_append(&JournalRecord::ExecDispatched {
            tenant: grant.tenant_id.clone(),
            sandbox: grant.sandbox_id.clone(),
            generation,
            execution: request.execution_id.clone(),
        })?;
        {
            let mut g = self.inner.lock().expect("service lock");
            let seq = g.journal.next_seq();
            let _ = g
                .registry
                .apply_exec_dispatched(&grant.tenant_id, &request.execution_id, seq);
        }
        let conn = self.link(&grant.sandbox_id).ok_or_else(|| {
            WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ExecutionNotStarted,
                "guest association lost",
            ))
        })?;
        // v1: worker always dispatches as uid/gid 1000; the guest clamps
        // root attempts regardless of what the wire claims.
        let spawn = WorkerToGuest::ExecSpawn {
            execution_id: request.execution_id.clone(),
            argv: request.argv.clone(),
            cwd: request.cwd.clone(),
            environment: request.environment.clone(),
            timeout_sec: request.timeout_sec,
            max_output_bytes: request.max_output_bytes,
            max_processes: request.max_processes,
            run_as_uid: 1000,
            run_as_gid: 1000,
        };
        self.send_guest(&grant.sandbox_id, &conn, spawn).await?;
        self.journal_append(&JournalRecord::ExecAcked {
            tenant: grant.tenant_id.clone(),
            sandbox: grant.sandbox_id.clone(),
            generation,
            execution: request.execution_id.clone(),
        })?;
        let mut g = self.inner.lock().expect("service lock");
        let seq = g.journal.next_seq();
        let _ = g
            .registry
            .apply_exec_acked(&grant.tenant_id, &request.execution_id, seq);
        Ok(WorkerReplyBody::ExecStarted {
            handle: ExecutionHandle {
                execution_id: request.execution_id.clone(),
                state: agentkit_sandbox_contracts::state::CommandState::Acked,
                replayed: false,
            },
        })
    }

    async fn on_exec_cancel(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
    ) -> WorkerResult<WorkerReplyBody> {
        let digest = RequestDigest::of_canonical_json(&canonical_body(execution_id)?);
        let (_, _, _) = self.verify_grant(
            &grant.tenant_id,
            &grant.sandbox_id,
            grant,
            SandboxCapability::ProcessExec,
            &digest,
        )?;
        let (state, uncertain) = self.with_registry(|reg| {
            let rec = reg.execution(&grant.tenant_id, execution_id)?;
            if rec.sandbox_id != grant.sandbox_id {
                return Err(WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::NotFound,
                    "no such execution for this tenant",
                )));
            }
            Ok((rec.state, rec.uncertain))
        })?;
        if let Some(conn) = self.link(&grant.sandbox_id) {
            // Best effort: cancel_requested != stopped (S14).
            let _ = self
                .send_guest(
                    &grant.sandbox_id,
                    &conn,
                    WorkerToGuest::ExecCancel {
                        execution_id: execution_id.clone(),
                    },
                )
                .await;
        }
        let mut g = self.inner.lock().expect("service lock");
        let _ = g
            .registry
            .mark_cancel_requested(&grant.tenant_id, execution_id);
        drop(g);
        Ok(WorkerReplyBody::ExecCancelled {
            out: agentkit_sandbox_contracts::api::CommandStateOut {
                execution_id: execution_id.clone(),
                command_state: state,
                uncertain,
            },
        })
    }

    fn on_exec_events(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
        after_sequence: u64,
    ) -> WorkerResult<WorkerReplyBody> {
        let digest =
            RequestDigest::of_canonical_json(&canonical_body(&(execution_id, after_sequence))?);
        let _ = self.verify_grant(
            &grant.tenant_id,
            &grant.sandbox_id,
            grant,
            SandboxCapability::ProcessExec,
            &digest,
        )?;
        let events = self.with_registry(|reg| {
            reg.events_after(&grant.tenant_id, execution_id, after_sequence)
        })?;
        Ok(WorkerReplyBody::Events { events })
    }

    // -----------------------------------------------------------------------
    // Workspace fs ops (grant-permitted guest paths only; forwarded)
    // -----------------------------------------------------------------------

    async fn send_guest(
        &self,
        _sandbox: &SandboxId,
        conn: &Arc<GuestConn>,
        msg: WorkerToGuest,
    ) -> WorkerResult<()> {
        conn.tx
            .send(msg)
            .map_err(|_| WorkerError::GuestLink("guest connection closed".into()))
    }

    async fn on_fs(
        &self,
        _request_id: &str,
        grant: &WorkspaceGrant,
        op: GuestFsOp,
        checked_paths: &[GuestPath],
        capability: SandboxCapability,
    ) -> WorkerResult<WorkerReplyBody> {
        // Every wire path is a validated GuestPath (serde re-validates); the
        // grant must permit each referenced path (S12).
        for p in checked_paths {
            if !grant.permits_path(p) {
                return Err(WorkerError::Contract(
                    SandboxError::new(
                        SandboxErrorCode::PolicyDenied,
                        "path outside grant permissions",
                    )
                    .with_sandbox(&grant.sandbox_id, Some(grant.instance_generation.0))
                    .with_detail("permits_path"),
                ));
            }
        }
        let digest = RequestDigest::of_canonical_json(&canonical_body(&op)?);
        let _ = self.verify_grant(
            &grant.tenant_id,
            &grant.sandbox_id,
            grant,
            capability,
            &digest,
        )?;
        let conn = self.link(&grant.sandbox_id).ok_or_else(|| {
            WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::Unavailable,
                "no guest association for this generation",
            ))
        })?;
        let waiter = conn.request_fs();
        self.send_guest(
            &grant.sandbox_id,
            &conn,
            WorkerToGuest::FsOp { operation: op },
        )
        .await?;
        let outcome = tokio::time::timeout(GUEST_OP_TIMEOUT, waiter)
            .await
            .map_err(|_| {
                WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::Unavailable,
                    "guest fs operation timed out",
                ))
            })?
            .map_err(|_| WorkerError::GuestLink("fs result channel dropped".into()))?;
        map_guest_outcome(outcome)
    }

    // -----------------------------------------------------------------------
    // Artifact staging (§11)
    // -----------------------------------------------------------------------

    fn stage_dir(&self, stage_id: &StageId) -> PathBuf {
        self.config
            .storage_root
            .join("stages")
            .join(stage_id.as_str())
    }

    fn on_stage_begin(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        manifest: &StageManifest,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        if manifest.files.len() > agentkit_sandbox_contracts::limits::MAX_STAGE_FILES {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "too many staged files",
            )));
        }
        if manifest.total_bytes > MAX_STAGED_TOTAL_BYTES {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "staged total exceeds limit",
            )));
        }
        let mut total = 0u64;
        for f in &manifest.files {
            agentkit_sandbox_contracts::api::validate_stage_relative_path(&f.relative_path)
                .map_err(|e| WorkerError::Protocol(format!("stage path: {e}")))?;
            total = total.saturating_add(f.size);
        }
        if total != manifest.total_bytes {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "manifest total_bytes does not match file sizes",
            )));
        }
        // Self-digest convention (single authority): the manifest's own
        // `manifest_digest` field is zeroed before canonicalization — the
        // value is not part of its own digest.
        let computed = agentkit_sandbox_contracts::canonical::manifest_digest_of(manifest)
            .map_err(|e| WorkerError::Protocol(format!("manifest canonical: {e}")))?;
        if computed != manifest.manifest_digest {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "manifest_digest does not match canonical manifest",
            )));
        }
        let seq = self.journal_append(&JournalRecord::StageBegin {
            tenant: envelope.tenant_id.clone(),
            sandbox: envelope.sandbox_id.clone(),
            generation: envelope.instance_generation,
            stage_id: manifest.stage_id.clone(),
            direction: manifest.direction,
            total_bytes: manifest.total_bytes,
            manifest_digest: manifest.manifest_digest.clone(),
            files: manifest
                .files
                .iter()
                .map(|f| f.relative_path.clone())
                .collect(),
        })?;
        let mut g = self.inner.lock().expect("service lock");
        g.registry.apply_stage_begin(
            envelope.tenant_id.clone(),
            envelope.sandbox_id.clone(),
            envelope.instance_generation,
            manifest.stage_id.clone(),
            manifest.direction,
            manifest.total_bytes,
            manifest.manifest_digest.clone(),
            manifest.files.clone(),
        )?;
        drop(g);
        std::fs::create_dir_all(self.stage_dir(&manifest.stage_id))
            .map_err(|e| WorkerError::Storage(format!("stage dir: {e}")))?;
        crate::util::set_dir_mode(&self.stage_dir(&manifest.stage_id), 0o700);
        let _ = seq;
        Ok(WorkerReplyBody::Staged {
            stage_id: manifest.stage_id.clone(),
        })
    }

    fn on_stage_chunk(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        stage_id: &StageId,
        sequence: u32,
        bytes_b64: &str,
        chunk_digest: &ArtifactDigest,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        // Chunks form a gapless global sequence 0,1,2,…: reject gaps,
        // reordering, and duplicates before touching disk (§11.1).
        let next_sequence = self.with_registry(|reg| {
            reg.stage(&envelope.tenant_id, stage_id)
                .map(|s| Some(s.chunks))
                .unwrap_or(None)
        });
        let Some(expected) = next_sequence else {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::NotFound,
                "stage not found for tenant",
            )));
        };
        if sequence != expected {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                format!("chunk sequence gap: expected {expected}, got {sequence}"),
            )));
        }
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, bytes_b64)
            .map_err(|_| WorkerError::Protocol("stage chunk base64".into()))?;
        if bytes.len() > MAX_STAGE_CHUNK_BYTES {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "stage chunk too large",
            )));
        }
        if ArtifactDigest::of_bytes(&bytes) != *chunk_digest {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "chunk digest mismatch",
            )));
        }
        let path = self
            .stage_dir(stage_id)
            .join(format!("chunk-{sequence:06}.bin"));
        std::fs::write(&path, &bytes)
            .map_err(|e| WorkerError::Storage(format!("chunk write: {e}")))?;
        // Batched (progress, not effect boundary): no fsync per chunk.
        self.journal_append(&JournalRecord::StageChunk {
            tenant: envelope.tenant_id.clone(),
            sandbox: envelope.sandbox_id.clone(),
            generation: envelope.instance_generation,
            stage_id: stage_id.clone(),
            sequence,
        })?;
        let mut g = self.inner.lock().expect("service lock");
        g.registry
            .apply_stage_chunk(&envelope.tenant_id, stage_id, bytes.len() as u64)?;
        Ok(WorkerReplyBody::Ack {})
    }

    fn on_stage_commit(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        stage_id: &StageId,
        manifest_digest: &ArtifactDigest,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        let (recorded_digest, bytes_received, total) = self.with_registry(|reg| {
            reg.stage(&envelope.tenant_id, stage_id)
                .map(|s| (s.manifest_digest.clone(), s.bytes_received, s.total_bytes))
        })?;
        if &recorded_digest != manifest_digest {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "commit manifest_digest mismatch",
            )));
        }
        if bytes_received != total {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "incomplete stage: byte total mismatch",
            )));
        }
        self.journal_append(&JournalRecord::StageCommit {
            tenant: envelope.tenant_id.clone(),
            sandbox: envelope.sandbox_id.clone(),
            generation: envelope.instance_generation,
            stage_id: stage_id.clone(),
            manifest_digest: manifest_digest.clone(),
        })?;
        self.journal_sync()?; // force the batched chunk records durable
        let mut g = self.inner.lock().expect("service lock");
        g.registry
            .apply_stage_commit(&envelope.tenant_id, stage_id)?;
        Ok(WorkerReplyBody::Staged {
            stage_id: stage_id.clone(),
        })
    }

    fn on_stage_abort(
        &self,
        envelope: &agentkit_sandbox_contracts::api::ControlEnvelope,
        stage_id: &StageId,
    ) -> WorkerResult<WorkerReplyBody> {
        self.check_envelope(envelope, SandboxCapability::SandboxManage)?;
        if !self.with_registry(|reg| reg.stage(&envelope.tenant_id, stage_id).is_ok()) {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::NotFound,
                "stage not found for tenant",
            )));
        }
        self.journal_append(&JournalRecord::StageAbort {
            tenant: envelope.tenant_id.clone(),
            sandbox: envelope.sandbox_id.clone(),
            generation: envelope.instance_generation,
            stage_id: stage_id.clone(),
        })?;
        let _ = std::fs::remove_dir_all(self.stage_dir(stage_id));
        let mut g = self.inner.lock().expect("service lock");
        g.registry
            .apply_stage_abort(&envelope.tenant_id, stage_id)?;
        Ok(WorkerReplyBody::Ack {})
    }

    async fn on_import_commit(
        &self,
        grant: &WorkspaceGrant,
        stage_id: &StageId,
        manifest_digest: &ArtifactDigest,
    ) -> WorkerResult<WorkerReplyBody> {
        let digest =
            RequestDigest::of_canonical_json(&canonical_body(&(stage_id, manifest_digest))?);
        let _ = self.verify_grant(
            &grant.tenant_id,
            &grant.sandbox_id,
            grant,
            SandboxCapability::ArtifactImport,
            &digest,
        )?;
        let (files, recorded) = self.with_registry(|reg| {
            let s = reg.stage(&grant.tenant_id, stage_id)?;
            if !s.committed || s.aborted {
                return Err(WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::PolicyDenied,
                    "stage not committed",
                )));
            }
            Ok((s.files.clone(), s.manifest_digest.clone()))
        })?;
        if &recorded != manifest_digest {
            return Err(WorkerError::Contract(
                SandboxError::new(
                    SandboxErrorCode::ArtifactInvalid,
                    "import manifest_digest mismatch",
                )
                .with_sandbox(&grant.sandbox_id, Some(grant.instance_generation.0)),
            ));
        }
        // Gather the host-staged byte stream (chunk files in sequence).
        let stream = self.read_stage_chunks(stage_id)?;
        if stream.len() as u64 != files.iter().map(|f| f.size).sum::<u64>() {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "staged bytes do not match manifest sizes",
            )));
        }

        let guest_files: Vec<agentkit_sandbox_contracts::api::StageFile> = files.clone();
        self.guest_fs(
            &grant.sandbox_id,
            GuestFsOp::ImportBegin {
                stage_id: stage_id.clone(),
                files: guest_files,
            },
        )
        .await?;
        let mut cursor = 0usize;
        for f in &files {
            let end = cursor + f.size as usize;
            let mut seq = 0u32;
            let mut off = cursor;
            while off < end {
                let take =
                    (end - off).min(agentkit_sandbox_contracts::limits::MAX_STAGE_CHUNK_BYTES);
                let bytes = &stream[off..off + take];
                self.guest_fs(
                    &grant.sandbox_id,
                    GuestFsOp::ImportData {
                        stage_id: stage_id.clone(),
                        relative_path: f.relative_path.clone(),
                        sequence: seq,
                        bytes_b64: crate::guestlink::b64(bytes),
                        chunk_digest: ArtifactDigest::of_bytes(bytes),
                    },
                )
                .await?;
                off += take;
                seq += 1;
            }
            cursor = end;
        }
        match self
            .guest_fs(
                &grant.sandbox_id,
                GuestFsOp::ImportEnd {
                    stage_id: stage_id.clone(),
                },
            )
            .await?
        {
            GuestOpOutcome::ImportOk { copied } => Ok(WorkerReplyBody::Imported { copied }),
            GuestOpOutcome::Failed { code, message } => Err(map_guest_failure(code, message)),
            _ => Err(WorkerError::GuestLink("unexpected import outcome".into())),
        }
    }

    /// One bounded guest fs round trip with timeout and outcome plumbing.
    async fn guest_fs(
        &self,
        sandbox_id: &SandboxId,
        operation: GuestFsOp,
    ) -> WorkerResult<GuestOpOutcome> {
        let conn = self.link(sandbox_id).ok_or_else(|| {
            WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::Unavailable,
                "no guest association",
            ))
        })?;
        let waiter = conn.request_fs();
        self.send_guest(sandbox_id, &conn, WorkerToGuest::FsOp { operation })
            .await?;
        tokio::time::timeout(GUEST_OP_TIMEOUT, waiter)
            .await
            .map_err(|_| WorkerError::GuestLink("guest fs op timed out".into()))?
            .map_err(|_| WorkerError::GuestLink("guest fs result dropped".into()))
    }

    fn read_stage_chunks(&self, stage_id: &StageId) -> WorkerResult<Vec<u8>> {
        let dir = self.stage_dir(stage_id);
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|e| WorkerError::Storage(format!("stage dir: {e}")))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("chunk-") && n.ends_with(".bin"))
            .collect();
        names.sort();
        let mut out = Vec::new();
        for n in names {
            let mut bytes = std::fs::read(dir.join(&n))
                .map_err(|e| WorkerError::Storage(format!("chunk read: {e}")))?;
            out.append(&mut bytes);
        }
        Ok(out)
    }

    async fn on_export_request(
        self: &Arc<Self>,
        grant: &WorkspaceGrant,
        stage_id: &StageId,
        paths: &[GuestPath],
    ) -> WorkerResult<WorkerReplyBody> {
        for p in paths {
            if !grant.permits_path(p) {
                return Err(WorkerError::Contract(
                    SandboxError::new(SandboxErrorCode::PolicyDenied, "export path outside grant")
                        .with_sandbox(&grant.sandbox_id, Some(grant.instance_generation.0)),
                ));
            }
        }
        let digest = RequestDigest::of_canonical_json(&canonical_body(&(stage_id, paths))?);
        let (generation, _, _) = self.verify_grant(
            &grant.tenant_id,
            &grant.sandbox_id,
            grant,
            SandboxCapability::ArtifactExport,
            &digest,
        )?;
        let manifest = match self
            .guest_fs(
                &grant.sandbox_id,
                GuestFsOp::ExportGather {
                    stage_id: stage_id.clone(),
                    paths: paths.to_vec(),
                },
            )
            .await?
        {
            GuestOpOutcome::ExportOk { manifest } => manifest,
            GuestOpOutcome::Failed { code, message } => {
                return Err(map_guest_failure(code, message));
            }
            _ => return Err(WorkerError::GuestLink("unexpected export outcome".into())),
        };

        // Worker-side manifest validation (guest output is untrusted):
        // sorted-unique paths, sane sizes, self-consistent digest.
        let mut total = 0u64;
        let mut prev: Option<&str> = None;
        for f in &manifest.files {
            agentkit_sandbox_contracts::api::validate_stage_relative_path(&f.relative_path)
                .map_err(|e| WorkerError::Protocol(format!("export path: {e}")))?;
            if prev.is_some_and(|p| p >= f.relative_path.as_str()) {
                return Err(WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::ArtifactInvalid,
                    "export manifest files must be sorted and unique",
                )));
            }
            prev = Some(&f.relative_path);
            total = total.saturating_add(f.size);
        }
        if total != manifest.total_bytes || total > MAX_STAGED_TOTAL_BYTES {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "export manifest totals invalid",
            )));
        }
        let expected_digest = agentkit_sandbox_contracts::canonical::manifest_digest_of(&manifest)
            .map_err(|e| WorkerError::Protocol(format!("manifest canonical: {e}")))?;
        if expected_digest != manifest.manifest_digest {
            return Err(WorkerError::Contract(SandboxError::new(
                SandboxErrorCode::ArtifactInvalid,
                "export manifest_digest does not match canonical manifest",
            )));
        }

        self.journal_append(&JournalRecord::StageBegin {
            tenant: grant.tenant_id.clone(),
            sandbox: grant.sandbox_id.clone(),
            generation,
            stage_id: stage_id.clone(),
            direction: agentkit_sandbox_contracts::api::StageDirection::Export,
            total_bytes: manifest.total_bytes,
            manifest_digest: manifest.manifest_digest.clone(),
            files: manifest
                .files
                .iter()
                .map(|f| f.relative_path.clone())
                .collect(),
        })?;
        {
            let mut g = self.inner.lock().expect("service lock");
            if g.registry.stage(&grant.tenant_id, stage_id).is_err() {
                let _ = g.registry.apply_stage_begin(
                    grant.tenant_id.clone(),
                    grant.sandbox_id.clone(),
                    generation,
                    stage_id.clone(),
                    agentkit_sandbox_contracts::api::StageDirection::Export,
                    manifest.total_bytes,
                    manifest.manifest_digest.clone(),
                    manifest.files.clone(),
                );
            }
        }
        std::fs::create_dir_all(self.stage_dir(stage_id))
            .map_err(|e| WorkerError::Storage(format!("stage dir: {e}")))?;
        crate::util::set_dir_mode(&self.stage_dir(stage_id), 0o700);

        // Fetch chunks; recompute per-file digests independently (S17: guest
        // claims are never authority) and journal each chunk.
        let mut global_seq: u32 = 0;
        for f in &manifest.files {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            let mut received = 0u64;
            let mut seq = 0u32;
            loop {
                let outcome = self
                    .guest_fs(
                        &grant.sandbox_id,
                        GuestFsOp::ExportFetch {
                            stage_id: stage_id.clone(),
                            relative_path: f.relative_path.clone(),
                            sequence: seq,
                        },
                    )
                    .await?;
                let (bytes, chunk_digest, last) = match outcome {
                    GuestOpOutcome::ExportDataOk {
                        bytes_b64,
                        chunk_digest,
                        last,
                    } => (crate::guestlink::unb64(&bytes_b64)?, chunk_digest, last),
                    GuestOpOutcome::Failed { code, message } => {
                        return Err(map_guest_failure(code, message));
                    }
                    _ => {
                        return Err(WorkerError::GuestLink(
                            "unexpected export data outcome".into(),
                        ));
                    }
                };
                if bytes.len() > agentkit_sandbox_contracts::limits::MAX_STAGE_CHUNK_BYTES {
                    return Err(WorkerError::Contract(SandboxError::new(
                        SandboxErrorCode::ArtifactInvalid,
                        "export chunk exceeds cap",
                    )));
                }
                if ArtifactDigest::of_bytes(&bytes) != chunk_digest {
                    return Err(WorkerError::Contract(SandboxError::new(
                        SandboxErrorCode::ArtifactInvalid,
                        format!("export chunk digest mismatch for {}", f.relative_path),
                    )));
                }
                hasher.update(&bytes);
                received += bytes.len() as u64;
                if received > f.size {
                    return Err(WorkerError::Contract(SandboxError::new(
                        SandboxErrorCode::ArtifactInvalid,
                        "export file overran manifest size",
                    )));
                }
                let path = self
                    .stage_dir(stage_id)
                    .join(format!("chunk-{global_seq:06}.bin"));
                std::fs::write(&path, &bytes)
                    .map_err(|e| WorkerError::Storage(format!("chunk write: {e}")))?;
                self.journal_append(&JournalRecord::StageChunk {
                    tenant: grant.tenant_id.clone(),
                    sandbox: grant.sandbox_id.clone(),
                    generation,
                    stage_id: stage_id.clone(),
                    sequence: global_seq,
                })?;
                global_seq += 1;
                {
                    let mut g = self.inner.lock().expect("service lock");
                    g.registry
                        .apply_stage_chunk(&grant.tenant_id, stage_id, bytes.len() as u64)?;
                }
                if last {
                    break;
                }
                seq += 1;
            }
            if received != f.size {
                return Err(WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::ArtifactInvalid,
                    format!("export file {} short", f.relative_path),
                )));
            }
            let hex: String =
                sha2::Sha256::finalize(hasher)
                    .iter()
                    .fold(String::new(), |mut a, b| {
                        a.push_str(&format!("{b:02x}"));
                        a
                    });
            let got = ArtifactDigest::from_hex(&hex)
                .map_err(|e| WorkerError::Internal(format!("digest shape: {e}")))?;
            if got != f.sha256 {
                return Err(WorkerError::Contract(SandboxError::new(
                    SandboxErrorCode::ArtifactInvalid,
                    format!("export file digest mismatch: {}", f.relative_path),
                )));
            }
        }

        self.journal_append(&JournalRecord::StageCommit {
            tenant: grant.tenant_id.clone(),
            sandbox: grant.sandbox_id.clone(),
            generation,
            stage_id: stage_id.clone(),
            manifest_digest: manifest.manifest_digest.clone(),
        })?;
        self.journal_sync()?;
        {
            let mut g = self.inner.lock().expect("service lock");
            let _ = g.registry.apply_stage_commit(&grant.tenant_id, stage_id);
        }
        Ok(WorkerReplyBody::Exported { manifest })
    }
}

/// Map a guest `Failed` stable code into the frozen contract error space.
fn map_guest_failure(code: String, message: String) -> WorkerError {
    let mapped = match code.as_str() {
        "path_escape" | "permission_denied" | "invalid_path" | "outside_root" | "denied" => {
            SandboxErrorCode::PolicyDenied
        }
        "digest_mismatch" | "corrupt" | "hash_mismatch" => SandboxErrorCode::ArtifactInvalid,
        "not_found" | "missing" => SandboxErrorCode::NotFound,
        "timeout" | "busy" | "unavailable" | "link_lost" => SandboxErrorCode::Unavailable,
        "too_large" | "quota" => SandboxErrorCode::ResourceExhausted,
        _ => SandboxErrorCode::ProtocolIncompatible,
    };
    WorkerError::GuestFailure {
        code,
        message: redact_cap(&message, 200),
        mapped,
    }
}

fn map_guest_outcome(outcome: GuestOpOutcome) -> WorkerResult<WorkerReplyBody> {
    Ok(match outcome {
        GuestOpOutcome::ReadOk { reply } => WorkerReplyBody::Read { reply },
        GuestOpOutcome::WriteOk { receipt } => WorkerReplyBody::Write { receipt },
        GuestOpOutcome::ListOk { reply } => WorkerReplyBody::List { reply },
        GuestOpOutcome::SearchOk { reply } => WorkerReplyBody::Search { reply },
        GuestOpOutcome::GitOk { reply } => WorkerReplyBody::Git { reply },
        GuestOpOutcome::ImportOk { copied } => WorkerReplyBody::Imported { copied },
        GuestOpOutcome::ExportOk { manifest } => WorkerReplyBody::Exported { manifest },
        GuestOpOutcome::Ack {} => WorkerReplyBody::Ack {},
        GuestOpOutcome::ExportDataOk { .. } => {
            return Err(WorkerError::Internal(
                "chunked export data is consumed inside the worker, never mapped".into(),
            ));
        }
        GuestOpOutcome::Failed { code, message } => return Err(map_guest_failure(code, message)),
    })
}

/// Dispatch return shape: a full direct message (HelloAck) or a reply body.
pub enum Direct {
    Ack(WorkerToHost),
    Body(WorkerReplyBody),
}

/// Canonical digest of a request that itself carries a `request_digest`
/// field: the field is normalized to `""` (the "unset" value) before
/// canonicalization, so host and worker can agree without circularity.
///
pub fn digest_of_request<T: serde::Serialize>(request: &T) -> WorkerResult<RequestDigest> {
    // Delegated to the single authority in contracts (host uses the same fn).
    agentkit_sandbox_contracts::canonical::request_digest_of(request)
        .map_err(|e| WorkerError::Internal(format!("request digest: {e}")))
}

/// Jailer id: `ak-<sanitized-sandbox-id>-g<n>`, charset `[A-Za-z0-9_-]`
/// (jailer restriction). MUST stay identical to `agentkit-sandbox-worker-fc`'s
/// `jailer_id`; a parity test exists in both crates.
fn jailer_id(sandbox: &SandboxId, generation: InstanceGeneration) -> String {
    format!(
        "ak-{}-g{}",
        sandbox
            .as_str()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            })
            .collect::<String>(),
        generation.0
    )
}

#[cfg(test)]
mod path_convention_tests {
    use super::*;

    #[test]
    fn jailer_id_sanitizes_dots_and_matches_fc_convention() {
        let s = SandboxId::new("sb.1_2-x").unwrap();
        assert_eq!(jailer_id(&s, InstanceGeneration(3)), "ak-sb-1_2-x-g3");
    }
}
