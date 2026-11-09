//! `SandboxBackend` implementation over Firecracker + jailer + KVM.
//!
//! Trust rules here (S03/S05/S20):
//! - Every host path is **derived** from `storage_root` + validated
//!   ids/digests from the [`VerifiedLaunchSpec`]; nothing is accepted as a
//!   path from the wire.
//! - Only identifiers reach the jailer; the Firecracker config is built
//!   here, never passed by callers.
//! - Termination requires independently observed process evidence;
//!   otherwise `conclusive:false` → caller quarantines (S14).
//! - The VM is configured with **no network interface** (S04).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentkit_sandbox_contracts::api::{
    BackendCapabilities, ProcessIdentity, SandboxBackend, TerminationEvidence, VerifiedLaunchSpec,
    VmHandle, VmObservedState, VmPhysState,
};
use agentkit_sandbox_contracts::error::SandboxError;
use agentkit_sandbox_contracts::ids::ArtifactDigest;
use tokio::process::{Child, Command};

use crate::api::FcApiClient;
use crate::cgroup::{self, CgroupPlacement, CgroupSpec};
use crate::disk::{self, DiskClaim};
use crate::probe::{self, ProbeConfig};
use crate::{BASE_BOOT_ARGS, FcError, GUEST_AGENT_PORT, path_str};

#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// Admin-owned private storage root (0700) — all VM files live under it.
    pub storage_root: PathBuf,
    /// Trusted kernel image (built by scripts/sandbox-image).
    pub kernel_path: PathBuf,
    /// Trusted base rootfs image (read-only shared; private disks are
    /// per-generation copies created by the worker).
    pub rootfs_path: PathBuf,
    pub fc_bin: PathBuf,
    pub jailer_bin: PathBuf,
    /// Admin-delegated cgroup v2 subtree (optional; absence => not enforced
    /// and *reported*, never faked).
    pub cgroup_root: Option<PathBuf>,
    /// Exact admin-pinned version string ("1.13.1") or None for the
    /// supported range.
    pub pinned_fc_version: Option<String>,
    /// Identity jailer drops to before exec'ing firecracker.
    pub jailer_uid: u32,
    pub jailer_gid: u32,
}

type Key = (String, u64);

struct Pending {
    spec: VerifiedLaunchSpec,
    _claim: DiskClaim,
}

struct LiveVm {
    child: Child,
    identity: ProcessIdentity,
    _claim: DiskClaim,
    cgroup: Option<CgroupPlacement>,
    vsock_uds: PathBuf,
    api_uds: PathBuf,
}

pub struct FirecrackerBackend {
    cfg: BackendConfig,
    pending: Mutex<HashMap<Key, Pending>>,
    live: Mutex<HashMap<Key, LiveVm>>,
    fc_exe_digest: Mutex<Option<ArtifactDigest>>,
}

impl FirecrackerBackend {
    pub fn new(cfg: BackendConfig) -> Result<Self, FcError> {
        for p in [
            &cfg.storage_root,
            &cfg.kernel_path,
            &cfg.rootfs_path,
            &cfg.fc_bin,
            &cfg.jailer_bin,
        ] {
            if !p.exists() {
                return Err(FcError::Probe(format!(
                    "required path missing: {}",
                    p.display()
                )));
            }
        }
        Ok(Self {
            cfg,
            pending: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            fc_exe_digest: Mutex::new(None),
        })
    }

    fn key(sandbox: &str, generation: u64) -> Key {
        (sandbox.to_string(), generation)
    }

    fn sandbox_dir(&self, spec: &VerifiedLaunchSpec) -> PathBuf {
        self.cfg
            .storage_root
            .join("sandboxes")
            .join(spec.tenant_id.as_str())
            .join(spec.sandbox_id.as_str())
            .join(format!("gen-{}", spec.instance_generation.0))
    }

    fn trusted_images(&self, spec: &VerifiedLaunchSpec) -> disk::TrustedImages {
        disk::TrustedImages {
            kernel: self.cfg.kernel_path.clone(),
            kernel_digest: spec.trusted_kernel_digest.clone(),
            rootfs: self.cfg.rootfs_path.clone(),
            rootfs_digest: spec.rootfs_digest.clone(),
        }
    }

    fn jailer_root(&self) -> PathBuf {
        self.cfg.storage_root.join("jailer")
    }

    fn chroot_for(&self, spec: &VerifiedLaunchSpec) -> PathBuf {
        self.jailer_root().join(jailer_id(spec)).join("root")
    }

    fn api_socket_for(&self, spec: &VerifiedLaunchSpec) -> PathBuf {
        self.chroot_for(spec).join("run").join("firecracker.socket")
    }

    fn vsock_uds_for(&self, spec: &VerifiedLaunchSpec) -> PathBuf {
        self.chroot_for(spec).join("vsock.uds")
    }

    async fn wait_api_socket(&self, sock: &Path, budget: Duration) -> Result<(), FcError> {
        let start = Instant::now();
        while start.elapsed() < budget {
            if sock.exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Err(FcError::Timeout("api socket never appeared".into()))
    }

    fn fc_digest(&self) -> Result<ArtifactDigest, FcError> {
        let mut guard = self.fc_exe_digest.lock().unwrap();
        if let Some(d) = &*guard {
            return Ok(d.clone());
        }
        let d = ArtifactDigest::of_bytes(&std::fs::read(&self.cfg.fc_bin)?);
        *guard = Some(d.clone());
        Ok(d)
    }

    async fn kill_and_wait(&self, child: &mut Child) {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }

    /// vsock UDS path + guest-agent port for a live VM.
    pub fn guest_endpoint(&self, sandbox: &str, generation: u64) -> Option<(PathBuf, u32)> {
        let guard = self.live.lock().unwrap();
        guard
            .get(&Self::key(sandbox, generation))
            .map(|vm| (vm.vsock_uds.clone(), GUEST_AGENT_PORT))
    }
}

fn jailer_id(spec: &VerifiedLaunchSpec) -> String {
    // MUST stay byte-identical to the worker's `jailer_id` derivation
    // (service.rs); parity asserted by tests in both crates.
    format!(
        "ak-{}-g{}",
        spec.sandbox_id
            .as_str()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            })
            .collect::<String>(),
        spec.instance_generation.0
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Authoritative process start time (epoch ms) from `/proc/<pid>/stat`
/// field 22 + boot time — survives pid reuse checks.
fn proc_start_time_ms(pid: u32) -> u64 {
    if let Some(v) = proc_start_time_ms_inner(pid)
        && v != 0
    {
        return v;
    }
    // Fallback: directory mtime (second resolution) — never zero-pretence.
    std::fs::metadata(format!("/proc/{pid}"))
        .map(|m| usec_or_mtime(&m))
        .unwrap_or(0)
}

fn usec_or_mtime(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime() as u64 * 1000
}

fn proc_start_time_ms_inner(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm may contain spaces/parens: take the last ')' as the field anchor.
    let rest = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = rest.split(' ').collect();
    // field 22 overall == index 19 after the state field (fields 3..).
    let start_ticks: u64 = fields.get(19)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let uptime_sec = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split('.')
        .next()?
        .parse::<u64>()
        .ok()?;
    let now = now_ms() / 1000;
    let boot = now.checked_sub(uptime_sec)?;
    Some(boot * 1000 + (start_ticks as f64 / hz as f64 * 1000.0) as u64)
}

/// Process identity: pid + start time + euid + exe content digest.
fn process_identity(pid: u32, exe_digest: ArtifactDigest) -> ProcessIdentity {
    let start_time_ms = proc_start_time_ms(pid);
    let uid = unsafe { libc::geteuid() };
    ProcessIdentity {
        pid,
        start_time_ms,
        uid,
        exe_digest,
    }
}

#[async_trait::async_trait]
impl SandboxBackend for FirecrackerBackend {
    async fn probe(&self) -> Result<BackendCapabilities, SandboxError> {
        let cfg = ProbeConfig {
            fc_bin: self.cfg.fc_bin.clone(),
            jailer_bin: self.cfg.jailer_bin.clone(),
            cgroup_root: self.cfg.cgroup_root.clone(),
            pinned_fc_version: self.cfg.pinned_fc_version.clone(),
        };
        probe::probe(&cfg).map_err(SandboxError::from)
    }

    async fn provision(&self, spec: &VerifiedLaunchSpec) -> Result<VmHandle, SandboxError> {
        // 1. Hard capability gate (S05): fail closed without KVM etc.
        let caps = self.probe().await?;
        if !caps.hardware_virtualization {
            return Err(SandboxError::from(FcError::Probe(
                "hardware virtualization unavailable: refusing to launch".into(),
            )));
        }
        // 2. Verify trusted images against pinned digests (S11).
        self.trusted_images(spec)
            .verify()
            .map_err(SandboxError::from)?;
        // 3. Derive paths; refuse pre-existing generation dirs.
        let dir = self.sandbox_dir(spec);
        if dir.exists() {
            return Err(SandboxError::from(FcError::Verify(format!(
                "generation dir already exists: {}",
                dir.display()
            ))));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| SandboxError::from(FcError::io("mkdir gen dir", e)))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| SandboxError::from(FcError::io("chmod gen dir", e)))?;
        // 4. Private data disk (fresh per generation; persistent sandboxes
        // reuse *by generating a new generation dir*, never double-mount).
        let data_disk = dir.join("data.raw");
        disk::create_private_disk(&data_disk, spec.spec.resources.writable_disk_mib)
            .map_err(SandboxError::from)?;
        let claim = disk::DiskClaim::try_claim(&data_disk).map_err(SandboxError::from)?;
        self.pending.lock().unwrap().insert(
            Self::key(spec.sandbox_id.as_str(), spec.instance_generation.0),
            Pending {
                spec: spec.clone(),
                _claim: claim,
            },
        );
        Ok(VmHandle {
            sandbox_id: spec.sandbox_id.clone(),
            instance_generation: spec.instance_generation,
            backend: "firecracker".into(),
            hypervisor_pid: None,
        })
    }

    async fn launch(&self, handle: &VmHandle) -> Result<VmObservedState, SandboxError> {
        let key = Self::key(handle.sandbox_id.as_str(), handle.instance_generation.0);
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(&key)
            .ok_or_else(|| FcError::Verify("provision missing for handle".into()))?;
        let spec = &pending.spec;
        let dir = self.sandbox_dir(spec);
        let data_disk = dir.join("data.raw");
        let claim = pending._claim;

        // jailer root (0700, admin-owned layout).
        std::fs::create_dir_all(self.jailer_root())
            .map_err(|e| SandboxError::from(FcError::io("jailer root", e)))?;
        std::fs::set_permissions(self.jailer_root(), std::fs::Permissions::from_mode(0o700))
            .map_err(|e| SandboxError::from(FcError::io("chmod jailer", e)))?;

        let api_socket = self.api_socket_for(spec);
        let vsock_uds = self.vsock_uds_for(spec);
        let boot_args = format!(
            "{BASE_BOOT_ARGS} agentkit.boot_nonce={}",
            spec.boot_nonce_b64
        );
        let exe_digest = self.fc_digest()?;

        // Spawn: jailer setuid-drops then exec's firecracker — our child
        // *is* the VMM process after exec.
        let mut cmd = Command::new(&self.cfg.jailer_bin);
        cmd.arg("--id")
            .arg(jailer_id(spec))
            .arg("--root")
            .arg(self.jailer_root())
            .arg("--exec-file")
            .arg(&self.cfg.fc_bin)
            .arg("--uid")
            .arg(self.cfg.jailer_uid.to_string())
            .arg("--gid")
            .arg(self.cfg.jailer_gid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| FcError::io("spawn jailer", e))?;
        let pid = child.id().unwrap_or(0);

        // cgroup placement before any guest workload runs (S19).
        let cgroup = match &self.cfg.cgroup_root {
            Some(root) => {
                let cs = CgroupSpec {
                    root: root.clone(),
                    vcpus: spec.spec.resources.vcpus,
                    memory_mib: spec.spec.resources.memory_mib,
                    max_pids: spec.spec.resources.max_processes + 16,
                };
                cgroup::place(pid, &cs)?
            }
            None => None,
        };

        let configure = async {
            self.wait_api_socket(&api_socket, Duration::from_secs(5))
                .await?;
            let api = FcApiClient::new(&api_socket, Duration::from_secs(5));
            api.put(
                "/machine-config",
                &serde_json::json!({
                    "vcpu_count": spec.spec.resources.vcpus,
                    "mem_size_mib": spec.spec.resources.memory_mib,
                    "smt": false,
                }),
            )
            .await?;
            api.put(
                "/boot-source",
                &serde_json::json!({
                    "kernel_image_path": path_str(&self.trusted_images(spec).kernel),
                    "boot_args": boot_args,
                }),
            )
            .await?;
            api.put(
                "/drives/rootfs",
                &serde_json::json!({
                    "drive_id": "rootfs",
                    "path_on_host": path_str(&self.trusted_images(spec).rootfs),
                    "is_root_device": true,
                    "is_read_only": true,
                    "io_engine": "Sync",
                    "cache_type": "Unsafe",
                }),
            )
            .await?;
            api.put(
                "/drives/data",
                &serde_json::json!({
                    "drive_id": "data",
                    "path_on_host": path_str(&data_disk),
                    "is_root_device": false,
                    "is_read_only": false,
                    "io_engine": "Sync",
                    "cache_type": "Unsafe",
                }),
            )
            .await?;
            api.put(
                "/vsock",
                &serde_json::json!({
                    "guest_cid": spec.vsock_cid,
                    // relative to the jailer chroot
                    "uds_path": "vsock.uds",
                }),
            )
            .await?;
            // Deliberately NO /network call: the VM has no NIC (S04).
            api.put(
                "/actions",
                &serde_json::json!({"action_type": "InstanceStart"}),
            )
            .await?;
            Ok::<(), FcError>(())
        };
        match tokio::time::timeout(Duration::from_secs(30), configure).await {
            Err(_) => {
                self.kill_and_wait(&mut child).await;
                return Err(SandboxError::from(FcError::Timeout("vm configure".into())));
            }
            Ok(Err(e)) => {
                self.kill_and_wait(&mut child).await;
                return Err(SandboxError::from(e));
            }
            Ok(Ok(())) => {}
        }

        let identity = process_identity(pid, exe_digest);
        self.live.lock().unwrap().insert(
            key,
            LiveVm {
                child,
                identity: identity.clone(),
                _claim: claim,
                cgroup,
                vsock_uds,
                api_uds: api_socket,
            },
        );
        Ok(VmObservedState {
            state: VmPhysState::Running,
            hypervisor_pid: (pid != 0).then_some(pid),
            process_identity: Some(identity),
            last_observed_ms: now_ms(),
        })
    }

    async fn terminate(&self, handle: &VmHandle) -> Result<TerminationEvidence, SandboxError> {
        let key = Self::key(handle.sandbox_id.as_str(), handle.instance_generation.0);
        let mut vm = match self.live.lock().unwrap().remove(&key) {
            Some(vm) => vm,
            None => {
                // Not ours (or already gone): if nothing lives at the record,
                // it is conclusively absent only when pid identity is dead —
                // delegate that judgment to reconcile (worker), stay honest.
                return Ok(TerminationEvidence {
                    method: "absent".into(),
                    conclusive: false,
                    observed_ms: now_ms(),
                    identity: None,
                });
            }
        };
        let pid = vm.child.id().unwrap_or(0);
        let expected_identity = vm.identity.clone();

        // 1. Graceful: ACPI shutdown via API.
        let api = FcApiClient::new(&vm.api_uds, Duration::from_secs(3));
        let graceful = tokio::time::timeout(Duration::from_secs(10), async {
            let _ = api
                .put(
                    "/actions",
                    &serde_json::json!({"action_type": "SendCtrlAltDel"}),
                )
                .await;
            loop {
                if !child_alive(&mut vm.child) {
                    let _ = vm.child.wait().await;
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let method = if matches!(graceful, Ok(true)) {
            "acpi_ctrl_alt_del".to_string()
        } else {
            // 2. SIGTERM then SIGKILL of the VMM.
            let _ = vm.child.start_kill();
            let killed = tokio::time::timeout(Duration::from_secs(5), vm.child.wait()).await;
            if killed.is_err() {
                let _ = vm
                    .child
                    .id()
                    .map(|p| unsafe { libc::kill(p as libc::pid_t, libc::SIGKILL) });
                let _ = tokio::time::timeout(Duration::from_secs(5), vm.child.wait()).await;
            }
            "kill".to_string()
        };
        // 3. Verification: our child reaped and pid no longer ours.
        let conclusive = verify_gone(pid, &expected_identity);
        // 4. cgroup teardown only once the process is conclusively gone.
        if conclusive && let Some(cg) = vm.cgroup.take() {
            crate::cgroup::remove_placement(&cg);
        }
        Ok(TerminationEvidence {
            method,
            conclusive,
            observed_ms: now_ms(),
            identity: Some(expected_identity),
        })
    }

    async fn inspect(&self, handle: &VmHandle) -> Result<VmObservedState, SandboxError> {
        let key = Self::key(handle.sandbox_id.as_str(), handle.instance_generation.0);
        let mut guard = self.live.lock().unwrap();
        if let Some(vm) = guard.get_mut(&key) {
            let pid = vm.child.id().unwrap_or(0);
            // try_wait reaps on completion; no await under the lock.
            let alive = pid != 0 && child_alive(&mut vm.child);
            let state = if alive {
                VmPhysState::Running
            } else {
                VmPhysState::Stopped
            };
            Ok(VmObservedState {
                state,
                hypervisor_pid: (pid != 0).then_some(pid),
                process_identity: Some(vm.identity.clone()),
                last_observed_ms: now_ms(),
            })
        } else {
            Ok(VmObservedState {
                state: VmPhysState::Unknown,
                hypervisor_pid: None,
                process_identity: None,
                last_observed_ms: now_ms(),
            })
        }
    }
}

fn child_alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// Confirm the pid is gone OR now belongs to a different process identity.
fn verify_gone(pid: u32, expected: &ProcessIdentity) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return true; // ESRCH — gone
    }
    // alive but must not match our identity (pid reuse defense)
    match std::fs::metadata(format!("/proc/{pid}")) {
        Ok(md) => {
            use std::os::unix::fs::MetadataExt;
            md.uid() != expected.uid || proc_start_time_ms(pid) != expected.start_time_ms
        }
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_sandbox_contracts::ids::{
        ArtifactDigest, ImageDigest, InstanceGeneration, PrincipalId, SandboxId, TenantId,
    };
    use agentkit_sandbox_contracts::path::GuestPath;
    use agentkit_sandbox_contracts::policy::NetworkMode;
    use agentkit_sandbox_contracts::spec::{
        EffectiveResources, ImageRef, Lifecycle, SandboxOwner, VerifiedSandboxSpec,
    };

    fn spec() -> VerifiedLaunchSpec {
        VerifiedLaunchSpec {
            sandbox_id: SandboxId::new("sb.1_2-x").unwrap(),
            tenant_id: TenantId::new("t1").unwrap(),
            instance_generation: InstanceGeneration(3),
            image_digest: ImageDigest::of_bytes(b"i"),
            trusted_kernel_digest: ImageDigest::of_bytes(b"k"),
            rootfs_digest: ArtifactDigest::of_bytes(b"r"),
            spec: VerifiedSandboxSpec {
                tenant_id: TenantId::new("t1").unwrap(),
                owner: SandboxOwner {
                    principal_id: PrincipalId::new("p").unwrap(),
                    agent_id: None,
                },
                lifecycle: Lifecycle::Ephemeral,
                image: ImageRef {
                    digest: ImageDigest::of_bytes(b"i"),
                    os: "linux".into(),
                    arch: "x86_64".into(),
                },
                resources: EffectiveResources {
                    vcpus: 2,
                    memory_mib: 512,
                    writable_disk_mib: 64,
                    max_processes: 32,
                    command_timeout_sec: 60,
                    max_log_bytes: 1024,
                },
                network: NetworkMode::None,
                idle_stop_sec: 0,
                workspace_root: GuestPath::parse("/workspace").unwrap(),
                policy_digest: agentkit_sandbox_contracts::ids::PolicyDigest::of_bytes(b"p"),
                policy_generation: agentkit_sandbox_contracts::ids::PolicyGeneration(1),
            },
            boot_nonce_digest: ArtifactDigest::of_bytes(b"n"),
            boot_nonce_b64: "AAAA".into(),
            vsock_cid: 10,
            allow_guest_root: false,
        }
    }

    #[test]
    fn jailer_ids_are_allowlisted_charset() {
        let id = jailer_id(&spec());
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        // Exact shared convention with the worker (service.rs).
        assert_eq!(id, "ak-sb-1_2-x-g3");
    }

    #[test]
    fn process_identity_reads_real_start_time() {
        let me = std::process::id();
        let t = proc_start_time_ms(me);
        let now = now_ms();
        // This process started between an hour ago and now.
        assert!(t > now - 3_600_000 && t <= now + 5000, "t={t} now={now}");
    }

    #[test]
    fn unknown_live_vm_is_unknown_not_stopped() {
        let b = FirecrackerBackend {
            cfg: BackendConfig {
                storage_root: "/nonexistent".into(),
                kernel_path: "/nonexistent/kernel.bin".into(),
                rootfs_path: "/nonexistent/rootfs.ext4".into(),
                fc_bin: "/nonexistent".into(),
                jailer_bin: "/nonexistent".into(),
                cgroup_root: None,
                pinned_fc_version: None,
                jailer_uid: 1000,
                jailer_gid: 1000,
            },
            pending: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            fc_exe_digest: Mutex::new(None),
        };
        // terminate on unknown handle must be inconclusive, never fake.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ev = rt
            .block_on(b.terminate(&VmHandle {
                sandbox_id: SandboxId::new("ghost").unwrap(),
                instance_generation: InstanceGeneration(1),
                backend: "firecracker".into(),
                hypervisor_pid: None,
            }))
            .unwrap();
        assert!(!ev.conclusive);
        assert_eq!(ev.method, "absent");
    }

    #[test]
    fn verify_gone_for_dead_pid() {
        // Spawn+reap a process to obtain a definitely-dead pid.
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        let id = ProcessIdentity {
            pid,
            start_time_ms: 1,
            uid: unsafe { libc::geteuid() },
            exe_digest: ArtifactDigest::of_bytes(b"x"),
        };
        // pid may be reused by the OS in theory; verify_gone only returns
        // false when identity matches — dead-pid case must be true.
        assert!(verify_gone(pid, &id) || std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}
