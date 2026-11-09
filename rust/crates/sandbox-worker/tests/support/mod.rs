//! Shared harness for sandbox-worker integration tests.
//!
//! Builds a `WorkerService` on a temporary storage root with the
//! `FakeBackend` and the in-process guest simulator, and speaks the real
//! `HostToWorker` catalog into `WorkerService::handle` (the same entry the
//! UDS transport uses after framing).

#![allow(dead_code)]
#![allow(unused_imports)]

pub mod sim_guest;

use agentkit_sandbox_contracts::api::{
    ControlEnvelope, ExecArgv, ExecRequest, StageFile, StageManifest, VerifiedLaunchSpec,
};
use agentkit_sandbox_contracts::grant::{GuestEnvironment, WorkspaceGrant};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, InstanceGeneration, PrincipalId, RequestDigest, SandboxId,
    StageId, TenantId, ToolCallId,
};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::{NetworkMode, SandboxCapability};
use agentkit_sandbox_contracts::protocol::WireFrame;
use agentkit_sandbox_contracts::spec::{
    ImageRef, Lifecycle, NetworkSpec, ResourceProfile, RetentionSpec, SandboxSpec, WorkspaceSpec,
};
use agentkit_sandbox_worker::backend::fake::{FakeBackend, FaultInjector};
use agentkit_sandbox_worker::config::{BackendKind, WorkerConfig};
use agentkit_sandbox_worker::guestlink::{DefaultGuestLinkFactory, SimBehavior};
use agentkit_sandbox_worker::registry::QuotaCeilings;
use agentkit_sandbox_worker::service::{WorkerService, digest_of_request};
use agentkit_sandbox_worker::{Journal, JournalRecord};
use std::path::Path;
use std::sync::Arc;

pub const POLICY_DIGEST_STR: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
pub const IMAGE_DIGEST_STR: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

pub fn tid(s: &str) -> TenantId {
    TenantId::new(s).unwrap()
}
pub fn sid(s: &str) -> SandboxId {
    SandboxId::new(s).unwrap()
}
pub fn eid(s: &str) -> ExecutionId {
    ExecutionId::new(s).unwrap()
}
pub fn prid(s: &str) -> PrincipalId {
    PrincipalId::new(s).unwrap()
}
pub fn stage_id(s: &str) -> StageId {
    StageId::new(s).unwrap()
}
pub fn gp(s: &str) -> GuestPath {
    GuestPath::parse(s).unwrap()
}
pub fn genof(n: u64) -> InstanceGeneration {
    InstanceGeneration::new(n).unwrap()
}
pub fn policy_digest() -> agentkit_sandbox_contracts::ids::PolicyDigest {
    agentkit_sandbox_contracts::ids::PolicyDigest::of_bytes(b"policy-v1")
}
pub fn policy_generation() -> agentkit_sandbox_contracts::ids::PolicyGeneration {
    agentkit_sandbox_contracts::ids::PolicyGeneration(1)
}
pub fn image_digest() -> agentkit_sandbox_contracts::ids::ImageDigest {
    agentkit_sandbox_contracts::ids::ImageDigest::of_bytes(b"image-v1")
}

pub struct Harness {
    pub tmp: tempfile::TempDir,
    pub svc: Arc<WorkerService>,
    pub fake: Arc<FakeBackend>,
    pub faults: Arc<FaultInjector>,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_ceilings(QuotaCeilings::default())
    }

    pub fn with_ceilings(ceilings: QuotaCeilings) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        Self::build(tmp, ceilings)
    }

    /// Reopen the same storage root (simulated daemon restart / SIGKILL
    /// recovery): journal is replayed into a fresh registry.
    pub fn reopen(tmp: tempfile::TempDir) -> Self {
        Self::build(tmp, QuotaCeilings::default())
    }

    fn build(tmp: tempfile::TempDir, ceilings: QuotaCeilings) -> Self {
        let ceilings = ceilings.clone();
        let faults = Arc::new(FaultInjector::new());
        let fake = Arc::new(FakeBackend::with_faults(Arc::clone(&faults)));
        let config = WorkerConfig {
            uds: tmp.path().join("run/worker.sock"),
            storage_root: tmp.path().join("storage"),
            peer_uids: vec![unsafe { libc::getuid() }],
            worker_id: "test-worker".into(),
            revision: 7,
            install_fingerprint:
                "sha256:3333333333333333333333333333333333333333333333333333333333333333".into(),
            backend: BackendKind::Fake,
            guest_vsock_port: 52,
            rootfs_path: None,
            kernel_path: None,
            fc_bin: None,
            jailer_bin: None,
            helper_path: None,
            cgroup_root: None,
            idle_sweep_sec: 0,
            ceilings: ceilings.clone(),
            broker_enabled: false,
            broker_test_origins: Vec::new(),
        };
        let svc = WorkerService::new(
            config,
            Arc::clone(&fake) as Arc<dyn agentkit_sandbox_contracts::api::SandboxBackend>,
            Arc::new(DefaultGuestLinkFactory),
            ceilings.clone(),
        )
        .unwrap();
        Self {
            tmp,
            svc,
            fake,
            faults,
        }
    }

    /// Rebuild the service on the SAME storage root (simulated crash):
    /// journal replay must rebuild the registry.
    pub fn restart(&self) -> Arc<WorkerService> {
        self.restart_with(Arc::new(FakeBackend::with_faults(Arc::new(
            FaultInjector::new(),
        ))))
    }

    /// Restart reusing the SAME physical backend (the realistic crash model:
    /// the worker process dies, VMs and disks survive).
    pub fn restart_with(&self, fake: Arc<FakeBackend>) -> Arc<WorkerService> {
        let faults = Arc::new(FaultInjector::new());
        let config = WorkerConfig {
            uds: self.tmp.path().join("run/worker2.sock"),
            storage_root: self.tmp.path().join("storage"),
            peer_uids: vec![unsafe { libc::getuid() }],
            worker_id: "test-worker".into(),
            revision: 8,
            install_fingerprint:
                "sha256:3333333333333333333333333333333333333333333333333333333333333333".into(),
            backend: BackendKind::Fake,
            guest_vsock_port: 52,
            rootfs_path: None,
            kernel_path: None,
            fc_bin: None,
            jailer_bin: None,
            helper_path: None,
            cgroup_root: None,
            idle_sweep_sec: 0,
            ceilings: QuotaCeilings::default(),
            broker_enabled: false,
            broker_test_origins: Vec::new(),
        };
        let _ = faults;
        WorkerService::new(
            config,
            fake as Arc<dyn agentkit_sandbox_contracts::api::SandboxBackend>,
            Arc::new(DefaultGuestLinkFactory),
            QuotaCeilings::default(),
        )
        .unwrap()
    }

    pub fn set_sim_behavior(&self, b: Option<SimBehavior>) {
        self.svc.set_sim_behavior(b);
    }

    /// Drive one request through the service entry point (framing layer is
    /// exercised separately in `uds_peer_auth`).
    pub async fn call(
        &self,
        request_id: &str,
        sequence: u64,
        msg: &agentkit_sandbox_contracts::api::messages::HostToWorker,
    ) -> agentkit_sandbox_contracts::api::messages::WorkerToHost {
        call_svc(&self.svc, request_id, sequence, msg).await
    }

    pub async fn ok(
        &self,
        request_id: &str,
        sequence: u64,
        msg: &agentkit_sandbox_contracts::api::messages::HostToWorker,
    ) -> agentkit_sandbox_contracts::api::messages::WorkerReplyBody {
        match self.call(request_id, sequence, msg).await {
            agentkit_sandbox_contracts::api::messages::WorkerToHost::Reply { outcome, .. } => {
                match outcome {
                    agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok { body } => body,
                    agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error } => {
                        panic!("unexpected worker error: {error:?}")
                    }
                }
            }
            other => panic!("expected reply, got {other:?}"),
        }
    }

    /// Call and capture the structured error.
    pub async fn err(
        &self,
        request_id: &str,
        sequence: u64,
        msg: &agentkit_sandbox_contracts::api::messages::HostToWorker,
    ) -> agentkit_sandbox_contracts::error::SandboxError {
        match self.call(request_id, sequence, msg).await {
            agentkit_sandbox_contracts::api::messages::WorkerToHost::Reply { outcome, .. } => {
                match outcome {
                    agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error } => error,
                    agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok { body } => {
                        panic!("expected error, got ok: {body:?}")
                    }
                }
            }
            other => panic!("expected reply, got {other:?}"),
        }
    }

    pub fn envelope(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        generation: InstanceGeneration,
        capability: SandboxCapability,
    ) -> ControlEnvelope {
        ControlEnvelope {
            tenant_id: tenant.clone(),
            principal_id: prid("principal"),
            sandbox_id: sandbox.clone(),
            instance_generation: generation,
            policy_generation: policy_generation(),
            policy_digest: policy_digest(),
            request_digest: RequestDigest::of_canonical_json(b"recorded-only"),
            capability,
        }
    }

    pub fn launch(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        generation: InstanceGeneration,
    ) -> (ControlEnvelope, VerifiedLaunchSpec) {
        let spec = SandboxSpec {
            version: agentkit_sandbox_contracts::SPEC_VERSION,
            owner: agentkit_sandbox_contracts::spec::SandboxOwner {
                principal_id: prid("principal"),
                agent_id: None,
            },
            lifecycle: Lifecycle::Ephemeral,
            image: ImageRef {
                digest: image_digest(),
                os: "linux".into(),
                arch: agentkit_sandbox_contracts::host_arch().into(),
            },
            resources: ResourceProfile {
                vcpus: 1,
                memory_mib: 256,
                writable_disk_mib: 1024,
                max_processes: 16,
                command_timeout_sec: 60,
                max_log_bytes: 1 << 20,
            },
            network: NetworkSpec {
                mode: NetworkMode::None,
                registry_policy_id: None,
            },
            retention: RetentionSpec { idle_stop_sec: 0 },
            workspace: WorkspaceSpec {
                root_in_guest: gp("/workspace"),
                host_mounts: Vec::new(),
            },
        };
        let boot_nonce = vec![0xa5u8; 32];
        let launch = VerifiedLaunchSpec {
            sandbox_id: sandbox.clone(),
            tenant_id: tenant.clone(),
            instance_generation: generation,
            image_digest: image_digest(),
            trusted_kernel_digest: image_digest(),
            rootfs_digest: ArtifactDigest::of_bytes(b"rootfs"),
            spec: agentkit_sandbox_contracts::spec::VerifiedSandboxSpec {
                tenant_id: tenant.clone(),
                owner: spec.owner.clone(),
                lifecycle: spec.lifecycle,
                image: spec.image.clone(),
                resources: agentkit_sandbox_contracts::spec::EffectiveResources {
                    vcpus: spec.resources.vcpus,
                    memory_mib: spec.resources.memory_mib,
                    writable_disk_mib: spec.resources.writable_disk_mib,
                    max_processes: spec.resources.max_processes,
                    command_timeout_sec: spec.resources.command_timeout_sec,
                    max_log_bytes: spec.resources.max_log_bytes,
                },
                network: NetworkMode::None,
                idle_stop_sec: 0,
                workspace_root: gp("/workspace"),
                policy_digest: policy_digest(),
                policy_generation: policy_generation(),
            },
            boot_nonce_digest: ArtifactDigest::of_bytes(&boot_nonce),
            boot_nonce_b64: base64_encode(&boot_nonce),
            vsock_cid: 3,
            allow_guest_root: false,
        };
        let env = self.envelope(
            tenant,
            sandbox,
            generation,
            SandboxCapability::SandboxManage,
        );
        (env, launch)
    }

    /// Create + reach Ready (create is synchronous through handshake in the
    /// fake path).
    pub async fn create_ready(&self, tenant: &str, sandbox: &str) {
        let t = tid(tenant);
        let s = sid(sandbox);
        let (env, launch) = self.launch(&t, &s, genof(1));
        let body = self
            .ok(
                "create-1",
                1,
                &agentkit_sandbox_contracts::api::messages::HostToWorker::CreateVm {
                    envelope: env,
                    launch,
                },
            )
            .await;
        assert!(
            matches!(
                body,
                agentkit_sandbox_contracts::api::messages::WorkerReplyBody::Created { .. }
            ),
            "create must succeed: {body:?}"
        );
    }

    pub fn grant(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        capability: SandboxCapability,
        intended_request_digest: RequestDigest,
        permitted_paths: Vec<GuestPath>,
        exec_cwd: Option<GuestPath>,
    ) -> WorkspaceGrant {
        WorkspaceGrant {
            tenant_id: tenant.clone(),
            principal_id: prid("principal"),
            session_id: agentkit_sandbox_contracts::ids::SessionId::new("sess-1").unwrap(),
            task_id: None,
            attempt_id: None,
            sandbox_id: sandbox.clone(),
            instance_generation: genof(1),
            policy_generation: policy_generation(),
            policy_digest: policy_digest(),
            capability,
            writer_lease: None,
            expires_at_ms: now_ms_future(),
            intended_request_digest,
            tool_call_id: Some(ToolCallId::new("tc-1").unwrap()),
            permitted_paths,
            exec_cwd,
        }
    }

    /// Grant carrying an explicit acting task and writer lease (S09
    /// mutating paths: `SandboxWrite`, `ArtifactImport`).
    #[allow(clippy::too_many_arguments)]
    pub fn grant_w(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        capability: SandboxCapability,
        intended_request_digest: agentkit_sandbox_contracts::ids::RequestDigest,
        permitted_paths: Vec<GuestPath>,
        exec_cwd: Option<GuestPath>,
        task: Option<agentkit_sandbox_contracts::ids::TaskId>,
        lease: Option<agentkit_sandbox_contracts::grant::WriterLeaseToken>,
    ) -> WorkspaceGrant {
        let mut g = self.grant(
            tenant,
            sandbox,
            capability,
            intended_request_digest,
            permitted_paths,
            exec_cwd,
        );
        g.task_id = task;
        g.writer_lease = lease;
        g
    }

    /// Admit the next single writer (S09) and return its lease token.
    pub async fn acquire_writer(
        &self,
        tenant: &TenantId,
        sandbox: &SandboxId,
        task: &agentkit_sandbox_contracts::ids::TaskId,
        ttl_ms: u64,
    ) -> agentkit_sandbox_contracts::grant::WriterLeaseToken {
        let envelope = self.envelope(tenant, sandbox, genof(1), SandboxCapability::SandboxManage);
        let msg = agentkit_sandbox_contracts::api::messages::HostToWorker::WriterAcquire {
            envelope,
            task_id: task.clone(),
            ttl_ms,
        };
        match self.ok("writer-acquire", 900, &msg).await {
            agentkit_sandbox_contracts::api::messages::WorkerReplyBody::WriterLease { token } => {
                token
            }
            other => panic!("writer acquire failed: {other:?}"),
        }
    }

    pub fn exec_request(&self, execution: &str, argv: &str) -> ExecRequest {
        ExecRequest {
            execution_id: eid(execution),
            tool_call_id: ToolCallId::new("tc-1").unwrap(),
            request_digest: RequestDigest::of_canonical_json(b"placeholder"),
            argv: ExecArgv::Direct {
                executable: argv.into(),
                args: Vec::new(),
            },
            cwd: gp("/workspace"),
            environment: GuestEnvironment::new(),
            timeout_sec: 30,
            max_output_bytes: 1 << 20,
            max_processes: 16,
        }
    }

    pub fn journal_records(&self) -> Vec<JournalRecord> {
        let path = self.svc.journal_path();
        Journal::replay(&path).unwrap()
    }
}

/// Journal record kind tag as persisted (serde `kind` tag).
pub fn kind_of(r: &JournalRecord) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// Poll an async condition (bounded).
pub async fn wait_for<F, Fut>(timeout_ms: u64, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if f().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

pub fn digest_of<T: serde::Serialize>(v: &T) -> RequestDigest {
    digest_of_request(v).unwrap()
}

pub fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn now_ms_future() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 600_000
}

pub fn storage_disk_path(h: &Harness, tenant: &str, sandbox: &str) -> std::path::PathBuf {
    h.tmp
        .path()
        .join("storage")
        .join("sandboxes")
        .join(tenant)
        .join(sandbox)
        .join("gen-1")
        .join("data.raw")
}

pub fn file_exists(p: &Path) -> bool {
    p.exists()
}

/// Drive one request through any service instance (framing layer is
/// exercised separately in `uds_peer_auth`).
pub async fn call_svc(
    svc: &Arc<WorkerService>,
    request_id: &str,
    sequence: u64,
    msg: &agentkit_sandbox_contracts::api::messages::HostToWorker,
) -> agentkit_sandbox_contracts::api::messages::WorkerToHost {
    let frame = WireFrame {
        header: agentkit_sandbox_contracts::protocol::FrameHeader {
            protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
            request_id: request_id.into(),
            sandbox_id: None,
            instance_generation: None,
            execution_id: None,
            operation: "test".into(),
            sequence,
            deadline_ms: None,
            payload_len: 0,
            payload_digest: None,
        },
        body: serde_json::to_value(msg).unwrap(),
    };
    svc.handle(&frame).await
}
