//! Host-side worker client implementing the frozen `SandboxManager` +
//! `SandboxWorkspaceIo` contracts over the worker's UDS framed protocol.
//!
//! Digest discipline (S07): this client is the only party that fills
//! `intended_request_digest`, `ExecRequest::request_digest`, and
//! `ControlEnvelope::request_digest`, immediately before dispatch, always
//! via [`agentkit_sandbox_contracts::canonical::request_digest_of`] — the
//! single authority shared with the worker verifier. The request body is
//! serialized first (digest fields normalized away by the authority), then
//! the digest is set into the request field where present.
//!
//! Tenant discipline (S02): everything the wire claims about tenancy is
//! derived from this client's [`ClientBinding`] — established from
//! server-side state, never from a model-supplied value. Inbound
//! `AuthContext`/`WorkspaceGrant` arguments are cross-checked against it
//! before anything is sent.
//!
//! Uncertainty discipline (S07/S14): a mutation whose frame reached the
//! socket but whose reply never arrived resolves to
//! `sandbox_execution_uncertain` (or `sandbox_termination_unverified` for
//! stop/destroy) and is never auto-retried. Idempotent reads retry at most
//! once over a fresh connection. There is no host-execution fallback on any
//! error path (S06).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base64::Engine as _;
use tokio::sync::Mutex;

use agentkit_sandbox_contracts::api::messages::{
    GuestFsOp, HostToWorker, WorkerReplyBody, WorkerToHost,
};
use agentkit_sandbox_contracts::api::{
    CapabilityManifest, CommandStateOut, ControlEnvelope, DeletionReceipt, ExecEvent, ExecRequest,
    ExecutionHandle, GitInspectQuery, GitInspectReply, ListOptions, ListReply, ReadReply,
    ReplyOutcomeShape, SandboxManager, SandboxRef, SandboxStateInfo, SandboxWorkspaceIo,
    SearchQuery, SearchReply, VerifiedLaunchSpec, VmPhysState, WriteReceipt,
};
use agentkit_sandbox_contracts::canonical::request_digest_of;
use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::grant::{AuthContext, WorkspaceGrant};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, ImageDigest, InstallFingerprint, InstanceGeneration, PolicyDigest,
    PolicyGeneration, PrincipalId, RequestDigest, SandboxId, SessionId, TenantId, WorkerId,
};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::{NetworkMode, SandboxCapability};
use agentkit_sandbox_contracts::spec::{
    AdmissionContext, EffectiveResources, Lifecycle, SandboxSpec, VerifiedSandboxSpec,
};
use agentkit_sandbox_contracts::state::SandboxState;
use agentkit_sandbox_contracts::{PROTOCOL_VERSION, SPEC_VERSION, host_arch};

use crate::conn::{self, Conn};

/// Host-local failures that never reach the wire (connect, peer auth, spawn).
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("worker connect failed: {0}")]
    Connect(String),
    #[error(
        "worker peer credential uid {actual} is not the expected {expected}; refusing the connection"
    )]
    PeerAuth { expected: u32, actual: u32 },
    #[error("worker handshake failed: {0}")]
    Handshake(String),
    #[error("worker process failure: {0}")]
    WorkerExited(String),
    #[error("io failure: {0}")]
    Io(#[from] std::io::Error),
}

/// How risky a request is if the socket dies after the frame was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Replay-safe query; may be retried once on a fresh connection.
    Idempotent,
    /// Workspace mutation (write): socket death => execution uncertain.
    Workspace,
    /// Command lifecycle (exec start/cancel): uncertain.
    Exec,
    /// VM lifecycle (create/start): uncertain until reconciled.
    Lifecycle,
    /// stop/destroy: termination/deletion unverified until reconciled.
    Termination,
}

impl Mutation {
    fn uncertain_error(self, op: &str) -> SandboxError {
        let code = match self {
            Mutation::Idempotent | Mutation::Lifecycle | Mutation::Workspace | Mutation::Exec => {
                SandboxErrorCode::ExecutionUncertain
            }
            Mutation::Termination => SandboxErrorCode::TerminationUnverified,
        };
        SandboxError::new(
            code,
            "worker connection lost after dispatch; outcome unknown — reconcile, never replay",
        )
        .with_detail(format!("op={op}"))
    }
}

/// Per-connection client configuration (all paths admin-derived, S20).
#[derive(Debug, Clone)]
pub struct WorkerClientConfig {
    pub uds: std::path::PathBuf,
    /// Expected peer uid from `SO_PEERCRED`; a mismatch refuses the socket.
    pub expected_peer_uid: u32,
    pub host_id: String,
    /// Protocol versions advertised on `Hello` (default `[PROTOCOL_VERSION]`).
    pub protocol_versions: Vec<u32>,
    /// Per-request wall timeout (frame deadline + reply wait).
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    /// Declared ceilings surfaced in `CapabilityManifest`.
    pub manifest_max_vcpus: u32,
    pub manifest_max_memory_mib: u32,
    /// Network modes this host advertises as supportable.
    pub network_modes: Vec<NetworkMode>,
    /// Trusted kernel + rootfs digests bound into the launch spec.
    pub trusted_kernel_digest: ImageDigest,
    pub rootfs_digest: ArtifactDigest,
    /// Fixed boot nonce for deterministic tests (None => `/dev/urandom`).
    pub boot_nonce_override: Option<Vec<u8>>,
}

impl WorkerClientConfig {
    #[must_use]
    pub fn new(uds: impl Into<std::path::PathBuf>, expected_peer_uid: u32) -> Self {
        Self {
            uds: uds.into(),
            expected_peer_uid,
            host_id: "agentkit-host".into(),
            protocol_versions: vec![PROTOCOL_VERSION],
            request_timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(5),
            manifest_max_vcpus: 16,
            manifest_max_memory_mib: 65_536,
            network_modes: vec![NetworkMode::None, NetworkMode::PackagesOnly],
            trusted_kernel_digest: ImageDigest::of_bytes(b"agentkit-trusted-kernel-v1"),
            rootfs_digest: ArtifactDigest::of_bytes(b"agentkit-rootfs-v1"),
            boot_nonce_override: None,
        }
    }
}

/// The authoritative identity this client instance is bound to; the only
/// source of tenancy claims on the wire (S02).
#[derive(Debug, Clone)]
pub struct ClientBinding {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub instance_generation: InstanceGeneration,
    pub policy_generation: PolicyGeneration,
    pub policy_digest: PolicyDigest,
    pub lifecycle: Lifecycle,
    pub image_digest: ImageDigest,
}

/// Server-advertised facts from the most recent HelloAck.
#[derive(Debug, Clone)]
pub struct HelloInfo {
    pub worker_id: WorkerId,
    pub revision: u64,
    pub install_fingerprint: InstallFingerprint,
    pub protocol_version: u32,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn zero_digest() -> RequestDigest {
    RequestDigest::of_canonical_json(b"")
}

fn protocol_mismatch(op: &str) -> SandboxError {
    SandboxError::new(
        SandboxErrorCode::ProtocolIncompatible,
        format!("worker returned an unexpected reply body for {op}"),
    )
}

/// One logical host<->worker association. Concurrent requests multiplex on
/// request id; reconnect happens transparently between requests.
pub struct WorkerClient {
    config: WorkerClientConfig,
    binding: ClientBinding,
    session: Mutex<Option<crate::conn::Session>>,
    hello_info: StdMutex<Option<HelloInfo>>,
    epoch: AtomicU64,
    reconnected: AtomicBool,
    request_counter: AtomicU64,
    /// Host-side bookkeeping of logical lifecycle state per sandbox (the
    /// wire surfaces physical state only; the worker keeps `SandboxState`
    /// in its registry).
    states: StdMutex<HashMap<SandboxId, SandboxState>>,
    /// Cached launch specs so `start` can replay `create` (no separate
    /// StartVm op exists; worker create is idempotent per generation).
    launches: StdMutex<HashMap<SandboxId, VerifiedLaunchSpec>>,
    effective: StdMutex<HashMap<SandboxId, EffectiveResources>>,
    host_nonce_counter: AtomicU64,
    admission: HostAdmission,
    /// Test hook: sever the socket right after the next frame write.
    arm_death_after_write: AtomicBool,
    /// Number of idempotent-request retries actually performed (never
    /// incremented for mutations — those must not retry, S07).
    retry_count: AtomicU64,
}

struct SessionHandle {
    conn: Arc<Conn>,
    #[allow(dead_code)]
    hello: HelloInfo,
}

fn read_boot_nonce(override_nonce: &Option<Vec<u8>>, counter: &AtomicU64) -> Vec<u8> {
    if let Some(n) = override_nonce {
        return n.clone();
    }
    let n = counter.fetch_add(1, Ordering::Relaxed);
    if let Ok(all) = std::fs::read("/dev/urandom")
        && all.len() >= 64
    {
        let off = (n as usize) % (all.len() - 32);
        return all[off..off + 32].to_vec();
    }
    (0..32u64)
        .map(|i| {
            let mix = n
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(i.wrapping_mul(1_442_695_040_888_963_407));
            (mix >> ((i % 8) * 8)) as u8
        })
        .collect()
}

impl WorkerClient {
    #[must_use]
    pub fn new(
        config: WorkerClientConfig,
        binding: ClientBinding,
        admission: HostAdmission,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            binding,
            admission,
            session: Mutex::new(None),
            hello_info: StdMutex::new(None),
            epoch: AtomicU64::new(0),
            reconnected: AtomicBool::new(false),
            request_counter: AtomicU64::new(0),
            states: StdMutex::new(HashMap::new()),
            launches: StdMutex::new(HashMap::new()),
            effective: StdMutex::new(HashMap::new()),
            host_nonce_counter: AtomicU64::new(0),
            arm_death_after_write: AtomicBool::new(false),
            retry_count: AtomicU64::new(0),
        })
    }

    /// Test hook: the next dispatched frame is followed by an abrupt socket
    /// death (the frame has fully reached the kernel socket, so the worker
    /// still processes it — exactly the crash-after-dispatch window).
    pub fn arm_socket_death_after_next_write(&self) {
        self.arm_death_after_write.store(true, Ordering::SeqCst);
    }

    /// Count of idempotent-request retries performed; mutations never
    /// increment it (they must not retry, S07).
    #[must_use]
    pub fn retry_count(&self) -> u64 {
        self.retry_count.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn binding(&self) -> &ClientBinding {
        &self.binding
    }

    #[must_use]
    pub fn hello_info(&self) -> Option<HelloInfo> {
        self.hello_info.lock().expect("hello lock").clone()
    }

    /// Increments on every established (connected + handshaked) association.
    #[must_use]
    pub fn connection_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// True once an established connection has died; biases reconciliation
    /// after a worker restart (physical `Unknown` stays uncertain).
    #[must_use]
    pub fn took_reconnect(&self) -> bool {
        self.reconnected.load(Ordering::SeqCst)
    }

    /// Establish the association eagerly (connect + Hello/HelloAck).
    pub async fn connect(&self) -> Result<HelloInfo, HostError> {
        let session = self.ensure_session().await?;
        Ok(session.hello)
    }

    /// Test hook: sever the socket abruptly (as a worker crash would),
    /// without touching the worker process. Pending mutations resolve
    /// uncertain; pending reads retry once.
    pub async fn simulate_socket_death(&self) {
        let mut guard = self.session.lock().await;
        if let Some(sess) = guard.take() {
            sess.conn.mark_dead();
            sess.reader.abort();
            self.reconnected.store(true, Ordering::SeqCst);
        }
    }

    /// Drain pushed execution events received since the last drain.
    pub fn drain_pushed_events(&self) -> Vec<ExecEvent> {
        match self.session.try_lock() {
            Ok(guard) => guard
                .as_ref()
                .map(|s| s.conn.drain_pushed())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    async fn ensure_session(&self) -> Result<SessionHandle, HostError> {
        let mut guard = self.session.lock().await;
        if let Some(sess) = guard.as_ref() {
            if !sess.conn.dead.load(Ordering::SeqCst) {
                return Ok(SessionHandle {
                    conn: Arc::clone(&sess.conn),
                    hello: sess.hello.clone(),
                });
            }
            self.reconnected.store(true, Ordering::SeqCst);
            sess.reader.abort();
            *guard = None;
        }
        let hello_id = self.next_request_id();
        let session = conn::establish(&self.config, &self.binding, hello_id).await?;
        self.epoch.fetch_add(1, Ordering::SeqCst);
        *self.hello_info.lock().expect("hello lock") = Some(session.hello.clone());
        let handle = SessionHandle {
            conn: Arc::clone(&session.conn),
            hello: session.hello.clone(),
        };
        *guard = Some(session);
        Ok(handle)
    }

    fn next_request_id(&self) -> String {
        let n = self.request_counter.fetch_add(1, Ordering::Relaxed);
        format!(
            "h{}-e{}-r{}",
            std::process::id(),
            self.epoch.load(Ordering::SeqCst),
            n
        )
    }

    /// Send one already-digest-filled request; correlate by request id.
    /// Idempotent requests may be retried once after connection death;
    /// mutations resolve uncertain instead (S07/S14).
    pub(crate) async fn request(
        &self,
        msg: &HostToWorker,
        mutation: Mutation,
        operation: &str,
        execution_id: Option<ExecutionId>,
    ) -> Result<WorkerReplyBody, SandboxError> {
        let max_attempts = if mutation == Mutation::Idempotent {
            2
        } else {
            1
        };
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let handle = match self.ensure_session().await {
                Ok(h) => h,
                Err(e) => {
                    // Connect failure: nothing reached the socket.
                    if mutation == Mutation::Idempotent && attempt < max_attempts {
                        continue;
                    }
                    return Err(SandboxError::new(
                        SandboxErrorCode::Unavailable,
                        format!("worker unavailable: {e}"),
                    ));
                }
            };
            let request_id = self.next_request_id();
            let bytes = conn::encode_request(
                &handle.conn,
                &self.config,
                &self.binding,
                &request_id,
                msg,
                operation,
                execution_id.clone(),
            )?;
            let rx = handle.conn.register(&request_id);
            if handle.conn.write_frame(&bytes).await.is_err() {
                // Partial-write semantics: bytes may already have reached
                // the peer. Treat as lost-after-write for mutations.
                handle.conn.mark_dead();
                if mutation != Mutation::Idempotent {
                    return Err(mutation.uncertain_error(operation));
                }
                if attempt < max_attempts {
                    self.retry_count.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                return Err(SandboxError::new(
                    SandboxErrorCode::Unavailable,
                    "worker connection lost before dispatch completed",
                )
                .with_detail(format!("op={operation}")));
            }
            if self.arm_death_after_write.swap(false, Ordering::SeqCst) {
                // Crash-after-dispatch window: the frame is in the socket,
                // the reply will never arrive on this connection.
                handle.conn.mark_dead();
            }
            match tokio::time::timeout(self.config.request_timeout, rx).await {
                Ok(Ok(Ok(WorkerToHost::Reply { outcome, .. }))) => match outcome {
                    ReplyOutcomeShape::Ok { body } => return Ok(body),
                    ReplyOutcomeShape::Err { error } => return Err(error),
                },
                Ok(Ok(Ok(_))) => {
                    handle.conn.remove(&request_id);
                    return Err(SandboxError::new(
                        SandboxErrorCode::ProtocolIncompatible,
                        "unexpected reply frame for request",
                    )
                    .with_detail(format!("op={operation}")));
                }
                // Dropped (connection died) or timed out: dispatch status
                // unknown after a successful write.
                Ok(Ok(Err(()))) | Ok(Err(_)) | Err(_) => {
                    handle.conn.remove(&request_id);
                    if mutation != Mutation::Idempotent {
                        return Err(mutation.uncertain_error(operation));
                    }
                    if attempt < max_attempts {
                        self.retry_count.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                    return Err(SandboxError::new(
                        SandboxErrorCode::Unavailable,
                        "worker did not answer an idempotent request after one retry",
                    )
                    .with_detail(format!("op={operation}")));
                }
            }
        }
    }

    // -- identity enforcement (S02) ----------------------------------------

    fn check_ctx(&self, ctx: &AuthContext, sandbox: &SandboxId) -> Result<(), SandboxError> {
        if ctx.tenant_id != self.binding.tenant_id
            || ctx.principal_id != self.binding.principal_id
            || sandbox != &self.binding.sandbox_id
        {
            // Indistinguishable from missing: no cross-tenant disclosure.
            return Err(SandboxError::new(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )
            .with_sandbox(
                &self.binding.sandbox_id,
                Some(self.binding.instance_generation.0),
            ));
        }
        if !ctx.has(SandboxCapability::SandboxManage) {
            return Err(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "auth context lacks sandbox_manage",
            ));
        }
        Ok(())
    }

    fn check_grant(&self, grant: &WorkspaceGrant) -> Result<(), SandboxError> {
        if grant.tenant_id != self.binding.tenant_id
            || grant.principal_id != self.binding.principal_id
        {
            return Err(SandboxError::new(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            )
            .with_sandbox(
                &self.binding.sandbox_id,
                Some(self.binding.instance_generation.0),
            ));
        }
        if grant.sandbox_id != self.binding.sandbox_id {
            return Err(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "grant references a sandbox outside this session binding",
            )
            .with_sandbox(
                &self.binding.sandbox_id,
                Some(self.binding.instance_generation.0),
            ));
        }
        if grant.instance_generation != self.binding.instance_generation {
            return Err(SandboxError::new(
                SandboxErrorCode::StaleGeneration,
                "grant references an old instance generation",
            )
            .with_sandbox(
                &self.binding.sandbox_id,
                Some(self.binding.instance_generation.0),
            ));
        }
        Ok(())
    }

    fn envelope(&self, capability: SandboxCapability) -> ControlEnvelope {
        ControlEnvelope {
            tenant_id: self.binding.tenant_id.clone(),
            principal_id: self.binding.principal_id.clone(),
            sandbox_id: self.binding.sandbox_id.clone(),
            instance_generation: self.binding.instance_generation,
            policy_generation: self.binding.policy_generation,
            policy_digest: self.binding.policy_digest.clone(),
            request_digest: zero_digest(),
            capability,
        }
    }

    /// THE digest authority for this crate: compute the digest over the
    /// serialized request (digest fields normalized by the shared
    /// authority), never a private copy.
    fn digest_of<T: serde::Serialize>(request: &T) -> Result<RequestDigest, SandboxError> {
        request_digest_of(request).map_err(|e| {
            SandboxError::new(
                SandboxErrorCode::ProtocolIncompatible,
                format!("request digest: {e}"),
            )
        })
    }

    /// Fill `grant.intended_request_digest` immediately before dispatch
    /// (callers pass the scaffold zero digest; anything else is ignored and
    /// replaced — the client is the sole digest authority, S07).
    fn seal_grant(
        &self,
        grant: &WorkspaceGrant,
        op: &impl serde::Serialize,
    ) -> Result<WorkspaceGrant, SandboxError> {
        let mut sealed = grant.clone();
        sealed.intended_request_digest = WorkerClient::digest_of(op)?;
        Ok(sealed)
    }

    fn record_state(&self, sandbox: &SandboxId, state: SandboxState) {
        self.states
            .lock()
            .expect("states lock")
            .insert(sandbox.clone(), state);
    }

    fn cached_state(&self, sandbox: &SandboxId) -> SandboxState {
        self.states
            .lock()
            .expect("states lock")
            .get(sandbox)
            .copied()
            .unwrap_or(SandboxState::Requested)
    }

    fn state_info(&self, sandbox: &SandboxId, physical: VmPhysState) -> SandboxStateInfo {
        let effective = self
            .effective
            .lock()
            .expect("effective lock")
            .get(sandbox)
            .cloned();
        let (v, m, d) = effective
            .map(|e| (e.vcpus, e.memory_mib, e.writable_disk_mib))
            .unwrap_or((0, 0, 0));
        let state = self.cached_state(sandbox);
        SandboxStateInfo {
            reference: SandboxRef {
                sandbox_id: sandbox.clone(),
                tenant_id: self.binding.tenant_id.clone(),
                lifecycle: self.binding.lifecycle,
                state,
                instance_generation: self.binding.instance_generation,
                image_digest: self.binding.image_digest.clone(),
                policy_digest: self.binding.policy_digest.clone(),
            },
            state,
            reserved_vcpus: v,
            reserved_memory_mib: m,
            reserved_disk_mib: d,
            active_executions: 0,
            writer_epoch: None,
            last_activity_ms: now_ms(),
            quarantine_reason: if state == SandboxState::Quarantined {
                Some("host-side: reconciliation pending".into())
            } else {
                None
            },
            physical_state: physical,
        }
    }
}

/// Host-side admission inputs used to validate + clamp a `SandboxSpec`
/// before it becomes a `VerifiedLaunchSpec` (§5.3; the worker independently
/// enforces the envelope<->launch binding).
#[derive(Debug, Clone)]
pub struct HostAdmission {
    pub ceilings: crate::contracts::limits::ResourceCeilings,
    pub policy: crate::contracts::policy::SandboxPolicy,
    /// Approved image digests (host image registry view).
    pub approved_images: std::collections::BTreeSet<ImageDigest>,
}

impl Default for HostAdmission {
    fn default() -> Self {
        Self {
            ceilings: crate::contracts::limits::ResourceCeilings::default(),
            policy: crate::contracts::policy::SandboxPolicy::default_tenant_policy(),
            approved_images: std::collections::BTreeSet::new(),
        }
    }
}

/// Host-side admission: validate + clamp the (untrusted) spec into the
/// verified form, injecting the authenticated tenant from the binding (S02).
pub(crate) fn verify_spec_for_launch(
    spec: &SandboxSpec,
    binding: &ClientBinding,
    admission: &HostAdmission,
) -> Result<VerifiedSandboxSpec, SandboxError> {
    if spec.version != SPEC_VERSION {
        return Err(SandboxError::new(
            SandboxErrorCode::ImageInvalid,
            format!("unsupported spec version {}", spec.version),
        ));
    }
    let ctx = AdmissionContext {
        ceilings: &admission.ceilings,
        policy: &admission.policy,
        approved_images: &admission.approved_images,
        host_arch: host_arch(),
    };
    spec.verify(&binding.tenant_id, &ctx).map_err(|e| {
        let code = match &e {
            crate::contracts::spec::SpecError::ImageNotApproved
            | crate::contracts::spec::SpecError::ImageInvalid(_) => SandboxErrorCode::ImageInvalid,
            crate::contracts::spec::SpecError::NetworkUnsupported(_) => {
                SandboxErrorCode::CapabilityUnsupported
            }
            crate::contracts::spec::SpecError::OverCeiling { .. } => {
                SandboxErrorCode::ResourceExhausted
            }
            _ => SandboxErrorCode::PolicyDenied,
        };
        SandboxError::new(code, e.to_string())
    })
}

#[async_trait::async_trait]
impl SandboxManager for WorkerClient {
    async fn capabilities(&self, ctx: &AuthContext) -> Result<CapabilityManifest, SandboxError> {
        self.check_ctx(ctx, &self.binding.sandbox_id)?;
        let body = self
            .request(
                &HostToWorker::GetCapabilities {},
                Mutation::Idempotent,
                "get_capabilities",
                None,
            )
            .await?;
        let caps = match body {
            WorkerReplyBody::Capabilities { capabilities } => capabilities,
            _ => return Err(protocol_mismatch("get_capabilities")),
        };
        let hello = self
            .hello_info()
            .ok_or_else(|| SandboxError::new(SandboxErrorCode::Unavailable, "not handshaked"))?;
        Ok(CapabilityManifest {
            backend: caps.backend,
            platform: std::env::consts::OS.to_string(),
            arch: host_arch().to_string(),
            protocol_version: hello.protocol_version,
            worker_revision: hello.revision,
            install_fingerprint: hello.install_fingerprint,
            features: caps.features,
            network_modes: self.config.network_modes.clone(),
            max_vcpus: self.config.manifest_max_vcpus,
            max_memory_mib: self.config.manifest_max_memory_mib,
            supported_image_arches: vec![host_arch().to_string()],
        })
    }

    async fn create(
        &self,
        ctx: &AuthContext,
        spec: &SandboxSpec,
    ) -> Result<SandboxRef, SandboxError> {
        if ctx.tenant_id != self.binding.tenant_id || ctx.principal_id != self.binding.principal_id
        {
            return Err(SandboxError::new(
                SandboxErrorCode::TenantForbidden,
                "no such sandbox for this tenant",
            ));
        }
        if !ctx.has(SandboxCapability::SandboxManage) {
            return Err(SandboxError::new(
                SandboxErrorCode::PolicyDenied,
                "auth context lacks sandbox_manage",
            ));
        }
        let verified = verify_spec_for_launch(spec, &self.binding, &self.admission)?;
        let nonce = read_boot_nonce(&self.config.boot_nonce_override, &self.host_nonce_counter);
        let launch = VerifiedLaunchSpec {
            sandbox_id: self.binding.sandbox_id.clone(),
            tenant_id: self.binding.tenant_id.clone(),
            instance_generation: self.binding.instance_generation,
            image_digest: self.binding.image_digest.clone(),
            trusted_kernel_digest: self.config.trusted_kernel_digest.clone(),
            rootfs_digest: self.config.rootfs_digest.clone(),
            spec: verified.clone(),
            boot_nonce_digest: ArtifactDigest::of_bytes(&nonce),
            boot_nonce_b64: base64::engine::general_purpose::STANDARD.encode(&nonce),
            vsock_cid: 3,
            allow_guest_root: false,
        };
        let body = self.dispatch_create(launch.clone()).await?;
        match body {
            WorkerReplyBody::Created { .. } => {
                self.record_state(&self.binding.sandbox_id, SandboxState::Ready);
                self.launches
                    .lock()
                    .expect("launches lock")
                    .insert(self.binding.sandbox_id.clone(), launch);
                self.effective
                    .lock()
                    .expect("effective lock")
                    .insert(self.binding.sandbox_id.clone(), verified.resources.clone());
                Ok(SandboxRef {
                    sandbox_id: self.binding.sandbox_id.clone(),
                    tenant_id: self.binding.tenant_id.clone(),
                    lifecycle: self.binding.lifecycle,
                    state: SandboxState::Ready,
                    instance_generation: self.binding.instance_generation,
                    image_digest: self.binding.image_digest.clone(),
                    policy_digest: self.binding.policy_digest.clone(),
                })
            }
            _ => Err(protocol_mismatch("create_vm")),
        }
    }

    async fn start(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<SandboxStateInfo, SandboxError> {
        self.check_ctx(ctx, id)?;
        let launch = self
            .launches
            .lock()
            .expect("launches lock")
            .get(id)
            .cloned()
            .ok_or_else(|| {
                SandboxError::new(
                    SandboxErrorCode::NotFound,
                    "no cached launch spec; create() first (or adopt via binding)",
                )
                .with_sandbox(id, Some(self.binding.instance_generation.0))
            })?;
        // No StartVm op exists on the wire: replay create (the worker is
        // idempotent per generation). A *stopped* VM is not relaunchable —
        // create replays the recorded creation, which the binding layer
        // surfaces as a reconcile decision.
        let body = self.dispatch_create(launch).await?;
        match body {
            WorkerReplyBody::Created { observed, .. } => {
                self.record_state(id, SandboxState::Ready);
                Ok(self.state_info(id, observed.state))
            }
            _ => Err(protocol_mismatch("create_vm")),
        }
    }

    async fn inspect(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<SandboxStateInfo, SandboxError> {
        self.check_ctx(ctx, id)?;
        let mut envelope = self.envelope(SandboxCapability::SandboxManage);
        let probe = HostToWorker::InspectVm {
            envelope: envelope.clone(),
        };
        envelope.request_digest = WorkerClient::digest_of(&probe)?;
        let msg = HostToWorker::InspectVm { envelope };
        match self
            .request(&msg, Mutation::Idempotent, "inspect_vm", None)
            .await?
        {
            WorkerReplyBody::Inspected { observed, .. } => Ok(self.state_info(id, observed.state)),
            _ => Err(protocol_mismatch("inspect_vm")),
        }
    }

    async fn stop(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<SandboxStateInfo, SandboxError> {
        self.check_ctx(ctx, id)?;
        let mut envelope = self.envelope(SandboxCapability::SandboxManage);
        let probe = HostToWorker::StopVm {
            envelope: envelope.clone(),
            force: false,
            reason: "host stop".into(),
        };
        envelope.request_digest = WorkerClient::digest_of(&probe)?;
        let msg = HostToWorker::StopVm {
            envelope,
            force: false,
            reason: "host stop".into(),
        };
        match self
            .request(&msg, Mutation::Termination, "stop_vm", None)
            .await?
        {
            WorkerReplyBody::Stopped { evidence } => {
                self.record_state(id, SandboxState::Stopped);
                let physical = if evidence.conclusive {
                    VmPhysState::Stopped
                } else {
                    VmPhysState::Unknown
                };
                Ok(self.state_info(id, physical))
            }
            _ => Err(protocol_mismatch("stop_vm")),
        }
    }

    async fn destroy(
        &self,
        ctx: &AuthContext,
        id: &SandboxId,
    ) -> Result<DeletionReceipt, SandboxError> {
        self.check_ctx(ctx, id)?;
        let mut envelope = self.envelope(SandboxCapability::SandboxManage);
        let probe = HostToWorker::DestroyVm {
            envelope: envelope.clone(),
            force: false,
        };
        envelope.request_digest = WorkerClient::digest_of(&probe)?;
        let msg = HostToWorker::DestroyVm {
            envelope,
            force: false,
        };
        match self
            .request(&msg, Mutation::Termination, "destroy_vm", None)
            .await?
        {
            WorkerReplyBody::Destroyed { receipt } => {
                self.record_state(id, SandboxState::Destroyed);
                self.launches.lock().expect("launches lock").remove(id);
                Ok(receipt)
            }
            _ => Err(protocol_mismatch("destroy_vm")),
        }
    }
}

impl WorkerClient {
    /// Single-writer admission for this session's sandbox (S09, M6):
    /// durable worker-side epoch bump; the returned token is what
    /// `PackEnv::vm_writer_lease` carries so mutating tool calls pass the
    /// worker's writer gate. Re-acquiring supersedes the previous holder
    /// (deterministic last-admission-wins; stale grants die).
    pub async fn acquire_writer(
        &self,
        ctx: &AuthContext,
        task_id: &agentkit_sandbox_contracts::ids::TaskId,
        ttl_ms: u64,
    ) -> Result<agentkit_sandbox_contracts::grant::WriterLeaseToken, SandboxError> {
        let id = self.binding.sandbox_id.clone();
        self.check_ctx(ctx, &id)?;
        let mut envelope = self.envelope(SandboxCapability::SandboxManage);
        let probe = HostToWorker::WriterAcquire {
            envelope: envelope.clone(),
            task_id: task_id.clone(),
            ttl_ms,
        };
        envelope.request_digest = WorkerClient::digest_of(&probe)?;
        let msg = HostToWorker::WriterAcquire {
            envelope,
            task_id: task_id.clone(),
            ttl_ms,
        };
        match self
            .request(&msg, Mutation::Lifecycle, "writer_acquire", None)
            .await?
        {
            WorkerReplyBody::WriterLease { token } => Ok(token),
            _ => Err(protocol_mismatch("writer_acquire")),
        }
    }

    /// Digest-fill and dispatch a `CreateVm` (shared by create/start).
    async fn dispatch_create(
        &self,
        launch: VerifiedLaunchSpec,
    ) -> Result<WorkerReplyBody, SandboxError> {
        let mut envelope = self.envelope(SandboxCapability::SandboxManage);
        let probe = HostToWorker::CreateVm {
            envelope: envelope.clone(),
            launch: launch.clone(),
        };
        // Request body serialized first; digest set into the envelope's
        // request_digest field immediately before dispatch.
        envelope.request_digest = WorkerClient::digest_of(&probe)?;
        let msg = HostToWorker::CreateVm { envelope, launch };
        self.request(&msg, Mutation::Lifecycle, "create_vm", None)
            .await
    }
}

#[async_trait::async_trait]
impl SandboxWorkspaceIo for WorkerClient {
    async fn read(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        from: Option<u64>,
        len: Option<u64>,
    ) -> Result<ReadReply, SandboxError> {
        self.check_grant(grant)?;
        let op = GuestFsOp::Read {
            path: path.clone(),
            from,
            len,
        };
        let grant = self.seal_grant(grant, &op)?;
        let msg = HostToWorker::FsRead {
            grant,
            path: path.clone(),
            from,
            len,
        };
        match self
            .request(&msg, Mutation::Idempotent, "fs_read", None)
            .await?
        {
            WorkerReplyBody::Read { reply } => Ok(reply),
            _ => Err(protocol_mismatch("fs_read")),
        }
    }

    async fn write(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        body: Vec<u8>,
        expected_sha256: Option<ArtifactDigest>,
    ) -> Result<WriteReceipt, SandboxError> {
        self.check_grant(grant)?;
        let body_b64 = base64::engine::general_purpose::STANDARD.encode(&body);
        // Mirror the worker's reconstruction exactly (it re-derives the op
        // with create_parents and digests that).
        let op = GuestFsOp::Write {
            path: path.clone(),
            body_b64: body_b64.clone(),
            expected_sha256: expected_sha256.clone(),
            create_parents: true,
        };
        let grant = self.seal_grant(grant, &op)?;
        let msg = HostToWorker::FsWrite {
            grant,
            path: path.clone(),
            body_b64,
            expected_sha256,
        };
        match self
            .request(&msg, Mutation::Workspace, "fs_write", None)
            .await?
        {
            WorkerReplyBody::Write { receipt } => Ok(receipt),
            _ => Err(protocol_mismatch("fs_write")),
        }
    }

    async fn list(
        &self,
        grant: &WorkspaceGrant,
        path: &GuestPath,
        options: ListOptions,
    ) -> Result<ListReply, SandboxError> {
        self.check_grant(grant)?;
        let op = GuestFsOp::List {
            path: path.clone(),
            options: options.clone(),
        };
        let grant = self.seal_grant(grant, &op)?;
        let msg = HostToWorker::FsList {
            grant,
            path: path.clone(),
            options,
        };
        match self
            .request(&msg, Mutation::Idempotent, "fs_list", None)
            .await?
        {
            WorkerReplyBody::List { reply } => Ok(reply),
            _ => Err(protocol_mismatch("fs_list")),
        }
    }

    async fn search(
        &self,
        grant: &WorkspaceGrant,
        query: SearchQuery,
    ) -> Result<SearchReply, SandboxError> {
        self.check_grant(grant)?;
        let op = GuestFsOp::Search {
            query: query.clone(),
        };
        let grant = self.seal_grant(grant, &op)?;
        let msg = HostToWorker::FsSearch { grant, query };
        match self
            .request(&msg, Mutation::Idempotent, "fs_search", None)
            .await?
        {
            WorkerReplyBody::Search { reply } => Ok(reply),
            _ => Err(protocol_mismatch("fs_search")),
        }
    }

    async fn git_inspect(
        &self,
        grant: &WorkspaceGrant,
        query: GitInspectQuery,
    ) -> Result<GitInspectReply, SandboxError> {
        self.check_grant(grant)?;
        let op = GuestFsOp::GitInspect {
            query: query.clone(),
        };
        let grant = self.seal_grant(grant, &op)?;
        let msg = HostToWorker::FsGitInspect { grant, query };
        match self
            .request(&msg, Mutation::Idempotent, "fs_git_inspect", None)
            .await?
        {
            WorkerReplyBody::Git { reply } => Ok(reply),
            _ => Err(protocol_mismatch("fs_git_inspect")),
        }
    }

    async fn exec(
        &self,
        grant: &WorkspaceGrant,
        mut request: ExecRequest,
    ) -> Result<ExecutionHandle, SandboxError> {
        self.check_grant(grant)?;
        // The exec request carries its own digest field: the body is
        // serialized first (normalized), then the digest is set into
        // `request_digest` and pinned onto the grant.
        let digest = WorkerClient::digest_of(&request)?;
        request.request_digest = digest.clone();
        let mut sealed = grant.clone();
        sealed.intended_request_digest = digest;
        let msg = HostToWorker::ExecStart {
            grant: sealed,
            request: Box::new(request.clone()),
        };
        let execution_id = request.execution_id.clone();
        match self
            .request(&msg, Mutation::Exec, "exec_start", Some(execution_id))
            .await?
        {
            WorkerReplyBody::ExecStarted { handle } => Ok(handle),
            _ => Err(protocol_mismatch("exec_start")),
        }
    }

    async fn exec_events(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
        after_sequence: u64,
    ) -> Result<Vec<ExecEvent>, SandboxError> {
        self.check_grant(grant)?;
        // Mirror of the worker verifier: digest over (execution_id,
        // after_sequence) exactly as `on_exec_events` recomputes it.
        let digest = WorkerClient::digest_of(&(execution_id.clone(), after_sequence))?;
        let mut sealed = grant.clone();
        sealed.intended_request_digest = digest;
        let msg = HostToWorker::ExecEvents {
            grant: sealed,
            execution_id: execution_id.clone(),
            after_sequence,
        };
        match self
            .request(
                &msg,
                Mutation::Idempotent,
                "exec_events",
                Some(execution_id.clone()),
            )
            .await?
        {
            WorkerReplyBody::Events { events } => Ok(events),
            _ => Err(protocol_mismatch("exec_events")),
        }
    }

    async fn exec_cancel(
        &self,
        grant: &WorkspaceGrant,
        execution_id: &ExecutionId,
    ) -> Result<CommandStateOut, SandboxError> {
        self.check_grant(grant)?;
        // Mirror of the worker verifier: digest over the execution id.
        let digest = WorkerClient::digest_of(execution_id)?;
        let mut sealed = grant.clone();
        sealed.intended_request_digest = digest;
        let msg = HostToWorker::ExecCancel {
            grant: sealed,
            execution_id: execution_id.clone(),
        };
        match self
            .request(
                &msg,
                Mutation::Exec,
                "exec_cancel",
                Some(execution_id.clone()),
            )
            .await?
        {
            WorkerReplyBody::ExecCancelled { out } => Ok(out),
            _ => Err(protocol_mismatch("exec_cancel")),
        }
    }
}

impl WorkerClient {
    /// Physical inventory for host reconciliation (§5.4) — the reconcile
    /// query of the frozen catalog.
    pub async fn reconcile_query(
        &self,
        ctx: &AuthContext,
    ) -> Result<crate::contracts::api::WorkerInventory, SandboxError> {
        self.check_ctx(ctx, &self.binding.sandbox_id)?;
        match self
            .request(
                &HostToWorker::ReconcileQuery {},
                Mutation::Idempotent,
                "reconcile_query",
                None,
            )
            .await?
        {
            WorkerReplyBody::Inventory { inventory } => Ok(inventory),
            _ => Err(protocol_mismatch("reconcile_query")),
        }
    }

    /// Raw `Hello` with an explicit version list (negotiation tests; the
    /// normal path always advertises the supported version).
    pub async fn hello_with_versions(&self, versions: Vec<u32>) -> Result<HelloInfo, HostError> {
        let mut cfg = self.config.clone();
        cfg.protocol_versions = versions;
        let session = conn::establish(&cfg, &self.binding, self.next_request_id()).await?;
        session.reader.abort();
        session.conn.mark_dead();
        Ok(session.hello)
    }
}
