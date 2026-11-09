//! Shared harness: spawns the real `agentkit-sandbox-worker` binary with the
//! fake backend over a private UDS in a temp dir, and builds typed
//! clients/grants for it.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;

use agentkit_sandbox_host::binding::SandboxBinding;
use agentkit_sandbox_host::client::{
    ClientBinding, HostAdmission, WorkerClient, WorkerClientConfig,
};
use agentkit_sandbox_host::contracts::grant::{AuthContext, WorkspaceGrant};
use agentkit_sandbox_host::contracts::ids::{
    ImageDigest, InstanceGeneration, PolicyDigest, PrincipalId, RequestDigest, SandboxId,
    SessionId, TenantId, ToolCallId,
};
use agentkit_sandbox_host::contracts::path::GuestPath;
use agentkit_sandbox_host::contracts::policy::SandboxCapability;
use agentkit_sandbox_host::contracts::spec::{
    ImageRef, Lifecycle, NetworkSpec, ResourceProfile, RetentionSpec, SandboxOwner, SandboxSpec,
    WorkspaceSpec,
};
use agentkit_sandbox_host::contracts::state::SandboxState;
use agentkit_sandbox_host::contracts::{SPEC_VERSION, host_arch};
use agentkit_sandbox_host::spawn::{SpawnConfig, SpawnedWorker, current_uid, spawn_worker};

pub const POLICY_DIGEST_BYTES: &[u8] = b"policy-v1";
pub const IMAGE_DIGEST_BYTES: &[u8] = b"image-v1";

/// The single tenant policy used by BOTH the host admission context and the
/// client bindings: `spec.verify` stamps `policy.compute_digest()` /
/// `policy.generation` into the launch spec, and the worker cross-checks the
/// control envelope against them (S02), so the binding must carry exactly
/// these values.
pub fn tenant_policy() -> agentkit_sandbox_host::contracts::policy::SandboxPolicy {
    agentkit_sandbox_host::contracts::policy::SandboxPolicy::default_tenant_policy()
}
pub const INSTALL_FINGERPRINT: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

pub fn image_digest() -> ImageDigest {
    ImageDigest::of_bytes(IMAGE_DIGEST_BYTES)
}
pub fn policy_digest() -> PolicyDigest {
    tenant_policy().compute_digest()
}
pub fn tid(s: &str) -> TenantId {
    TenantId::new(s).unwrap()
}
pub fn prid(s: &str) -> PrincipalId {
    PrincipalId::new(s).unwrap()
}
pub fn sid(s: &str) -> SandboxId {
    SandboxId::new(s).unwrap()
}
pub fn sess(s: &str) -> SessionId {
    SessionId::new(s).unwrap()
}
pub fn gp(s: &str) -> GuestPath {
    GuestPath::parse(s).unwrap()
}
pub fn genof(n: u64) -> InstanceGeneration {
    InstanceGeneration::new(n).unwrap()
}

pub fn worker_bin() -> PathBuf {
    // Priority: explicit override (integration owner), the package's own
    // faithful testbin (set by `cargo test -p agentkit-sandbox-host`), then
    // any externally provided worker binary.
    for var in [
        "AGENTKIT_TEST_WORKER_BIN",
        "CARGO_BIN_EXE_agentkit-sandbox-worker-testbin",
        "CARGO_BIN_EXE_agentkit_sandbox_worker_testbin",
        "CARGO_BIN_EXE_agentkit-sandbox-worker",
        "CARGO_BIN_EXE_agentkit_sandbox_worker",
    ] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            let p = PathBuf::from(&v);
            if p.exists() {
                return p;
            }
        }
    }
    // Last resort: sibling binary in the shared target directory.
    let exe = std::env::current_exe().unwrap_or_default();
    for dir in exe.ancestors() {
        let cand = dir.join("agentkit-sandbox-worker");
        if cand.is_file() {
            return cand;
        }
        if dir.join("CACHEDIR.TAG").is_file() {
            break;
        }
    }
    panic!("worker binary not found (set AGENTKIT_TEST_WORKER_BIN)");
}

pub fn spawn_config(tmp: &Path) -> SpawnConfig {
    SpawnConfig {
        worker_bin: worker_bin(),
        uds: tmp.join("run").join("worker.uds"),
        storage_root: tmp.join("storage"),
        peer_uid: current_uid(),
        expected_worker_uid: current_uid(),
        worker_id: "test-worker".into(),
        install_fingerprint: INSTALL_FINGERPRINT.into(),
        backend: "fake".into(),
        extra_args: Vec::new(),
        startup_timeout: std::time::Duration::from_secs(15),
    }
}

pub fn client_config(tmp: &Path) -> WorkerClientConfig {
    let mut cfg = WorkerClientConfig::new(tmp.join("run").join("worker.uds"), current_uid());
    cfg.boot_nonce_override = Some(vec![7u8; 32]);
    cfg.request_timeout = std::time::Duration::from_secs(20);
    cfg
}

pub fn binding_for(tenant: &str, principal: &str, session: &str, sandbox: &str) -> ClientBinding {
    ClientBinding {
        tenant_id: tid(tenant),
        principal_id: prid(principal),
        session_id: sess(session),
        sandbox_id: sid(sandbox),
        instance_generation: genof(1),
        policy_generation: tenant_policy().generation,
        policy_digest: policy_digest(),
        lifecycle: Lifecycle::Ephemeral,
        image_digest: image_digest(),
    }
}

pub fn admission() -> HostAdmission {
    HostAdmission {
        ceilings: Default::default(),
        policy: tenant_policy(),
        approved_images: [image_digest()].into_iter().collect(),
    }
}

pub fn spec() -> SandboxSpec {
    SandboxSpec {
        version: SPEC_VERSION,
        owner: SandboxOwner {
            principal_id: prid("alice"),
            agent_id: None,
        },
        lifecycle: Lifecycle::Ephemeral,
        image: ImageRef {
            digest: image_digest(),
            os: "linux".into(),
            arch: host_arch().to_string(),
        },
        resources: ResourceProfile::default(),
        network: NetworkSpec {
            mode: agentkit_sandbox_host::contracts::policy::NetworkMode::None,
            registry_policy_id: None,
        },
        retention: RetentionSpec { idle_stop_sec: 0 },
        workspace: WorkspaceSpec {
            root_in_guest: gp("/workspace"),
            host_mounts: Vec::new(),
        },
    }
}

pub fn manage_ctx(tenant: &str, principal: &str, session: &str) -> AuthContext {
    AuthContext {
        tenant_id: tid(tenant),
        principal_id: prid(principal),
        session_id: sess(session),
        agent_id: None,
        task_id: None,
        attempt_id: None,
        capabilities: [SandboxCapability::SandboxManage].into_iter().collect(),
        authorization_reason: "test lifecycle".into(),
    }
}

/// Scaffold grant exactly as `VmSandboxSession::grant_scaffold` mints it:
/// zero digest — the client is the only party allowed to fill it.
pub fn scaffold_grant(b: &ClientBinding, capability: SandboxCapability) -> WorkspaceGrant {
    WorkspaceGrant {
        tenant_id: b.tenant_id.clone(),
        principal_id: b.principal_id.clone(),
        session_id: b.session_id.clone(),
        task_id: None,
        attempt_id: None,
        sandbox_id: b.sandbox_id.clone(),
        instance_generation: b.instance_generation,
        policy_generation: b.policy_generation,
        policy_digest: b.policy_digest.clone(),
        capability,
        writer_lease: None,
        expires_at_ms: now_ms() + 600_000,
        intended_request_digest: RequestDigest::of_canonical_json(b""),
        tool_call_id: Some(ToolCallId::new("tc-1").unwrap()),
        permitted_paths: vec![gp("/workspace")],
        exec_cwd: Some(gp("/workspace")),
    }
}

/// Scaffold a mutating grant the way the host bridge does it (S09): the
/// acting `task` is the admitted writer holder and carries the worker
/// minted lease.
pub fn scaffold_write_grant(
    b: &ClientBinding,
    capability: SandboxCapability,
    task: &agentkit_sandbox_contracts::ids::TaskId,
    lease: &agentkit_sandbox_contracts::grant::WriterLeaseToken,
) -> WorkspaceGrant {
    let mut g = scaffold_grant(b, capability);
    g.task_id = Some(task.clone());
    g.writer_lease = Some(lease.clone());
    g
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Test environment: temp dir + running worker process.
pub struct TestEnv {
    pub tmp: TempDir,
    pub worker: SpawnedWorker,
}

impl TestEnv {
    pub async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let worker = spawn_worker(
            spawn_config(tmp.path()),
            client_config(tmp.path()),
            binding_for("probe", "probe", "probe", "sb-probe"),
            admission(),
        )
        .await
        .expect("worker spawns and handshakes");
        Self { tmp, worker }
    }

    /// Fresh client for a tenant identity (new UDS association).
    pub async fn client(
        &self,
        tenant: &str,
        principal: &str,
        session: &str,
        sandbox: &str,
    ) -> Arc<WorkerClient> {
        let c = WorkerClient::new(
            client_config(self.tmp.path()),
            binding_for(tenant, principal, session, sandbox),
            admission(),
        );
        c.connect().await.expect("client handshake");
        c
    }

    pub async fn restart_worker(&mut self) {
        let _ = self.worker.kill();
        let _ = self.worker.wait().await;
        self.worker = spawn_worker(
            spawn_config(self.tmp.path()),
            client_config(self.tmp.path()),
            binding_for("probe", "probe", "probe", "sb-probe"),
            admission(),
        )
        .await
        .expect("worker respawns");
    }

    pub fn journal_text(&self) -> String {
        let path = self
            .tmp
            .path()
            .join("storage")
            .join("journal")
            .join("worker-test-worker.jsonl");
        std::fs::read_to_string(path).unwrap_or_default()
    }

    pub fn count_in_journal(&self, needle: &str) -> usize {
        self.journal_text()
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }
}

pub fn deterministic_sandbox(tenant: &str, session: &str) -> SandboxId {
    SandboxBinding::deterministic_sandbox_id(&tid(tenant), &sess(session))
}

pub const READY_STATE: SandboxState = SandboxState::Ready;
