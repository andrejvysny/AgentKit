//! Virtualization.framework backend: drives the admin-pinned Swift helper
//! as one child process per VM (mirrors the Firecracker backend's
//! fail-closed semantics, spec §12.2).

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest as Sha2Digest, Sha256};
use tokio::process::{Child, Command};

use agentkit_sandbox_contracts::api::{
    BackendCapabilities, CapabilitySupport, ProcessIdentity, SandboxBackend, TerminationEvidence,
    VerifiedLaunchSpec, VmHandle, VmObservedState, VmPhysState,
};
use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::ids::{ArtifactDigest, InstanceGeneration, SandboxId};

use crate::proto::{write_frame, HelperCmd, HelperError, HelperProbe, HelperRequest, VmManifest};
use crate::BASE_BOOT_ARGS;

#[derive(Debug, thiserror::Error)]
pub enum VzError {
    #[error("io: {context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("verify: {0}")]
    Verify(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("helper: {0}")]
    Helper(#[from] HelperError),
}

impl From<VzError> for SandboxError {
    fn from(e: VzError) -> Self {
        match &e {
            VzError::Unavailable(m) => {
                SandboxError::new(SandboxErrorCode::IsolationUnavailable, m.clone())
            }
            VzError::Verify(m) => SandboxError::new(SandboxErrorCode::ImageInvalid, m.clone()),
            VzError::Io { context, source } => SandboxError::new(
                SandboxErrorCode::Unavailable,
                format!("{context}: {source}"),
            ),
            VzError::Helper(HelperError::Unavailable(m)) => {
                SandboxError::new(SandboxErrorCode::IsolationUnavailable, m.clone())
            }
            VzError::Helper(_) => SandboxError::new(SandboxErrorCode::Unavailable, e.to_string()),
        }
    }
}

fn io_err(context: &str, e: std::io::Error) -> VzError {
    VzError::Io {
        context: context.to_string(),
        source: e,
    }
}

#[derive(Debug, Clone)]
pub struct VzBackendConfig {
    /// Admin-owned private storage root (0700).
    pub storage_root: PathBuf,
    /// Trusted kernel (VZLinuxBootLoader-compatible ELF).
    pub kernel_path: PathBuf,
    /// Trusted read-only rootfs image.
    pub rootfs_path: PathBuf,
    /// Admin-pinned Swift helper binary.
    pub helper_path: PathBuf,
    /// Exact helper version we refuse to run below.
    pub min_helper_version: (u32, u32, u32),
    /// Guest vsock relay port override (default 52).
    pub guest_vsock_port: u32,
}

struct Pending {
    spec: VerifiedLaunchSpec,
    _claim: Claim,
}

/// Single-writer claim on a generation dir (parity with the FC disk claim).
struct Claim {
    lock: PathBuf,
}

impl Claim {
    fn try_claim(path: &Path) -> Result<Claim, VzError> {
        let lock = path.with_extension("lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => Ok(Claim { lock }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(VzError::Verify("data disk already claimed".into()))
            }
            Err(e) => Err(io_err("claim", e)),
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock);
    }
}

struct LiveVm {
    child: Child,
    identity: ProcessIdentity,
    _claim: Claim,
    relay_socket: PathBuf,
}

pub struct AppleVzBackend {
    cfg: VzBackendConfig,
    pending: Mutex<BTreeMap<String, Pending>>,
    live: Mutex<BTreeMap<String, LiveVm>>,
}

/// Defense-in-depth: a generation dir that already booted once must never
/// re-request data-disk formatting (crash-safe persistent path).
fn is_first_boot(dir: &Path) -> bool {
    !dir.join("booted.marker").exists()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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

fn sha256_of_file(path: &Path) -> Result<ArtifactDigest, VzError> {
    let bytes = std::fs::read(path).map_err(|e| io_err("read image", e))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    ArtifactDigest::from_hex(&hex(&h.finalize()))
        .map_err(|e| VzError::Verify(format!("digest shape: {e}")))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn version_lt(a: (u32, u32, u32), b: (u32, u32, u32)) -> bool {
    a < b
}

impl AppleVzBackend {
    pub fn new(cfg: VzBackendConfig) -> Result<Self, VzError> {
        for p in [&cfg.kernel_path, &cfg.rootfs_path, &cfg.helper_path] {
            if !p.is_absolute() {
                return Err(VzError::Verify(format!(
                    "admin path must be absolute: {}",
                    p.display()
                )));
            }
        }
        Ok(Self {
            cfg,
            pending: Mutex::new(BTreeMap::new()),
            live: Mutex::new(BTreeMap::new()),
        })
    }

    fn key(sandbox: &str, generation: u64) -> String {
        format!("{sandbox}#{generation}")
    }

    /// Worker-derived layout parity: the service expects the guest relay at
    /// `<storage_root>/jailer/<jailer-id>/root/vsock.uds`.
    fn vm_dir(&self, sandbox: &SandboxId, generation: InstanceGeneration) -> PathBuf {
        self.cfg
            .storage_root
            .join("jailer")
            .join(jailer_id(sandbox, generation))
            .join("root")
    }

    async fn run_helper_probe(&self) -> Result<HelperProbe, VzError> {
        if !self.cfg.helper_path.exists() {
            return Err(VzError::Unavailable(format!(
                "virtualization helper missing at {}",
                self.cfg.helper_path.display()
            )));
        }
        let mut cmd = Command::new(&self.cfg.helper_path);
        cmd.arg("probe")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .env_clear();
        // Preserve only what dyld needs; the helper never inherits tenant env.
        if let Some(p) = std::env::var_os("PATH") {
            cmd.env("PATH", p);
        }
        let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
            .await
            .map_err(|_| VzError::Unavailable("helper probe timed out".into()))?
            .map_err(|e| io_err("spawn helper probe", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(VzError::Unavailable(format!(
                "helper probe exited {}: {}",
                out.status.code().unwrap_or(-1),
                if err.is_empty() { "no stderr" } else { &err }
            )));
        }
        let probe: HelperProbe = serde_json::from_slice(&out.stdout)
            .map_err(|e| VzError::Unavailable(format!("helper probe json: {e}")))?;
        Ok(probe)
    }
}

#[async_trait::async_trait]
impl SandboxBackend for AppleVzBackend {
    async fn probe(&self) -> Result<BackendCapabilities, SandboxError> {
        let mut notes = Vec::new();
        let mut hw = false;
        match self.run_helper_probe().await {
            Ok(p) => {
                hw = p.hardware_virtualization && p.entitled;
                notes.extend(p.notes);
                notes.push(format!("helper version {}", p.helper_version));
                if !p.entitled {
                    notes.push("helper lacks the hypervisor entitlement".into());
                }
                if let Some(v) = parse_version(&p.helper_version) {
                    if version_lt(v, self.cfg.min_helper_version) {
                        notes.push(format!(
                            "helper {} below minimum {}.{}.{}",
                            p.helper_version,
                            self.cfg.min_helper_version.0,
                            self.cfg.min_helper_version.1,
                            self.cfg.min_helper_version.2
                        ));
                        hw = false;
                    }
                } else {
                    notes.push(format!("unparseable helper version {:?}", p.helper_version));
                    hw = false;
                }
            }
            Err(e) => notes.push(e.to_string()),
        }
        let mut features = BTreeMap::new();
        features.insert(
            "persistent_disk".into(),
            CapabilitySupport {
                supported: true,
                enforced: false,
                evidence: Some("sandbox-worker-apple::parity".into()),
            },
        );
        features.insert(
            "package_broker".into(),
            CapabilitySupport {
                supported: true,
                enforced: true,
                evidence: Some("sandbox-worker::broker_flow".into()),
            },
        );
        features.insert(
            "snapshot".into(),
            CapabilitySupport {
                supported: false,
                enforced: false,
                evidence: None,
            },
        );
        Ok(BackendCapabilities {
            backend: "apple-vz".into(),
            hardware_virtualization: hw,
            kvm_device: false,
            vsock: hw, // host-relayed virtio-vsock (helper)
            cgroups_v2: false,
            jailer: None,
            firecracker_version: None,
            features,
            notes,
        })
    }

    async fn provision(&self, spec: &VerifiedLaunchSpec) -> Result<VmHandle, SandboxError> {
        // 1. Hard capability gate (S05): fail closed without VZ/entitlement.
        let caps = self.probe().await?;
        if !caps.hardware_virtualization {
            return Err(SandboxError::new(
                SandboxErrorCode::IsolationUnavailable,
                format!(
                    "Virtualization.framework unavailable: {}",
                    caps.notes.join("; ")
                ),
            ));
        }
        // 2. Verify trusted images against pinned digests (S11) BEFORE the
        // helper is ever told about them.
        let kernel = sha256_of_file(&self.cfg.kernel_path)?;
        if kernel.as_str() != spec.trusted_kernel_digest.as_str() {
            return Err(VzError::Verify(format!(
                "kernel digest mismatch: pinned {} found {}",
                spec.trusted_kernel_digest.as_str(),
                kernel.as_str()
            ))
            .into());
        }
        let rootfs = sha256_of_file(&self.cfg.rootfs_path)?;
        if rootfs != spec.rootfs_digest {
            return Err(VzError::Verify(format!(
                "rootfs digest mismatch: pinned {} found {}",
                spec.rootfs_digest.as_str(),
                rootfs.as_str()
            ))
            .into());
        }
        let exe = sha256_of_file(&self.cfg.helper_path)?;
        let _ = exe; // recorded in launch identity below

        // 3. Derive paths; refuse pre-existing generation dirs.
        let dir = self.vm_dir(&spec.sandbox_id, spec.instance_generation);
        if dir.exists() {
            return Err(VzError::Verify(format!(
                "generation dir already exists: {}",
                dir.display()
            ))
            .into());
        }
        std::fs::create_dir_all(&dir).map_err(|e| io_err("mkdir gen dir", e))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io_err("chmod gen dir", e))?;

        // 4. Private data disk: zero sparse file; the initramfs formats it
        // on FIRST boot only (`agentkit.format_data=1`, appended at launch
        // from the marker below). No host mkfs exists on macOS.
        let data_disk = dir.join("data.raw");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&data_disk)
            .map_err(|e| io_err("create data disk", e))?;
        file.set_len(u64::from(spec.spec.resources.writable_disk_mib) * 1024 * 1024)
            .map_err(|e| io_err("sparse data disk", e))?;
        drop(file);
        let claim = Claim::try_claim(&data_disk)?;

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
            backend: "apple-vz".into(),
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
            .ok_or_else(|| VzError::Verify("provision missing for handle".into()))?;
        let spec = &pending.spec;
        let dir = self.vm_dir(&spec.sandbox_id, spec.instance_generation);
        let data_disk = dir.join("data.raw");
        let claim = pending._claim;
        let first_boot = is_first_boot(&dir);

        let relay_socket = dir.join("vsock.uds");
        let _ = std::fs::remove_file(&relay_socket);
        let boot_args = format!(
            "{BASE_BOOT_ARGS} agentkit.boot_nonce={}{}",
            spec.boot_nonce_b64,
            if first_boot {
                " agentkit.format_data=1"
            } else {
                ""
            }
        );
        let manifest = VmManifest {
            schema: 1,
            vm_id: jailer_id(&spec.sandbox_id, spec.instance_generation),
            kernel: self.cfg.kernel_path.to_string_lossy().into_owned(),
            boot_args,
            rootfs: self.cfg.rootfs_path.to_string_lossy().into_owned(),
            data_disk: data_disk.to_string_lossy().into_owned(),
            cpus: spec.spec.resources.vcpus,
            memory_mib: u64::from(spec.spec.resources.memory_mib),
            relay_socket: relay_socket.to_string_lossy().into_owned(),
            guest_vsock_port: self.cfg.guest_vsock_port,
        };
        let manifest_path = dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("manifest serializes"),
        )
        .map_err(|e| io_err("write manifest", e))?;
        std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| io_err("chmod manifest", e))?;

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("vm.log"))
            .map_err(|e| io_err("open vm log", e))?;
        let log_err = log.try_clone().map_err(|e| io_err("clone vm log", e))?;
        let mut child = Command::new(&self.cfg.helper_path)
            .arg("vm")
            .arg("--manifest")
            .arg(&manifest_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .env_clear()
            .process_group(0)
            .spawn()
            .map_err(|e| io_err("spawn helper", e))?;
        let pid = child.id().unwrap_or(0);
        let start_time_ms = now_ms();
        let exe_digest = sha256_of_file(&self.cfg.helper_path)?;
        let identity = ProcessIdentity {
            pid,
            start_time_ms,
            uid: unsafe { libc::geteuid() },
            exe_digest,
        };

        eprintln!("SPAWNED pid={} relay={:?}", pid, relay_socket);
        // The helper creates the relay socket once its virtio-vsock listener
        // is up; a fast helper death must surface as an error, not a hang.
        let wait = async {
            loop {
                if !child_alive(&mut child) {
                    return Err(VzError::Unavailable(
                        "helper exited before the vsock relay came up (see vm.log)".into(),
                    ));
                }
                eprintln!(
                    "POLL alive={} relay_exists={} trywait={:?}",
                    child_alive(&mut child),
                    relay_socket.exists(),
                    child.try_wait()
                );
                if relay_socket.exists() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        match tokio::time::timeout(Duration::from_secs(60), wait).await {
            Ok(Ok(())) => {}
            // Helper died pre-relay or timed out: the VM is not running —
            // reap and fail closed; never record a phantom Running VM.
            Ok(Err(_)) | Err(_) => {
                let _ = kill_and_wait(&mut child).await;
                let reason = "vsock relay did not come up (helper exited or timed out; see vm.log)";
                return Err(SandboxError::from(VzError::Unavailable(reason.into())));
            }
        }
        std::fs::write(dir.join("booted.marker"), b"1").map_err(|e| io_err("booted marker", e))?;

        self.live.lock().unwrap().insert(
            key,
            LiveVm {
                child,
                identity: identity.clone(),
                _claim: claim,
                relay_socket,
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
                return Ok(TerminationEvidence {
                    method: "absent".into(),
                    conclusive: false,
                    observed_ms: now_ms(),
                    identity: None,
                });
            }
        };
        let identity = vm.identity.clone();
        // 1. Ask the helper to force-stop the VM and exit. A reaped child
        // (we are the parent: waitpid is independent observation) is the
        // only basis for `conclusive`.
        let mut method = "helper_stop".to_string();
        let stopped = if let Some(mut stdin) = vm.child.stdin.take() {
            let req = HelperRequest {
                id: 1,
                cmd: HelperCmd::Stop {},
            };
            let sent = write_frame(&mut stdin, &req).await.is_ok();
            drop(stdin);
            sent
        } else {
            false
        };
        let reaped = if stopped {
            match tokio::time::timeout(Duration::from_secs(10), vm.child.wait()).await {
                Ok(Ok(_)) => true,
                // 2. SIGKILL the helper (the VZ VM dies with its process).
                _ => {
                    method = "kill".into();
                    kill_and_wait(&mut vm.child).await
                }
            }
        } else {
            method = "kill".into();
            kill_and_wait(&mut vm.child).await
        };
        // 3. Belt-and-braces: never claim conclusive while alive.
        let conclusive = reaped && !child_alive(&mut vm.child);
        let _ = std::fs::remove_file(&vm.relay_socket);
        Ok(TerminationEvidence {
            method,
            conclusive,
            observed_ms: now_ms(),
            identity: Some(identity),
        })
    }

    async fn inspect(&self, handle: &VmHandle) -> Result<VmObservedState, SandboxError> {
        let key = Self::key(handle.sandbox_id.as_str(), handle.instance_generation.0);
        let mut guard = self.live.lock().unwrap();
        if let Some(vm) = guard.get_mut(&key) {
            let pid = vm.child.id().unwrap_or(0);
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

async fn kill_and_wait(child: &mut Child) -> bool {
    let _ = child.start_kill();
    matches!(
        tokio::time::timeout(Duration::from_secs(5), child.wait()).await,
        Ok(Ok(_))
    )
}

fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let t = s.trim().trim_start_matches('v');
    let mut it = t.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next().unwrap_or("0").split('-').next()?.parse().ok()?;
    Some((a, b, c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_sandbox_contracts::ids::{
        ImageDigest, PolicyDigest, PolicyGeneration, PrincipalId, TenantId,
    };
    use agentkit_sandbox_contracts::path::GuestPath;
    use agentkit_sandbox_contracts::policy::NetworkMode;
    use agentkit_sandbox_contracts::spec::{
        EffectiveResources, ImageRef, Lifecycle, SandboxOwner, VerifiedSandboxSpec,
    };

    #[test]
    fn jailer_id_sanitizes_like_worker() {
        let s = SandboxId::new("sb.1_2-x").unwrap();
        assert_eq!(jailer_id(&s, InstanceGeneration(3)), "ak-sb-1_2-x-g3");
    }

    #[test]
    fn first_boot_marker_semantics() {
        let dir = std::env::temp_dir().join(format!("ak-apple-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(is_first_boot(&dir));
        std::fs::write(dir.join("booted.marker"), b"1").unwrap();
        assert!(!is_first_boot(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn version_ordering() {
        assert!(version_lt((1, 9, 0), (1, 10, 0)));
        assert!(!version_lt((2, 0, 0), (1, 9, 9)));
        assert_eq!(parse_version("v1.13.1"), Some((1, 13, 1)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("garbage"), None);
    }

    #[tokio::test]
    async fn probe_without_helper_is_unavailable() {
        let b = AppleVzBackend::new(VzBackendConfig {
            storage_root: PathBuf::from("/tmp/ak-apple-test"),
            kernel_path: PathBuf::from("/kernel"),
            rootfs_path: PathBuf::from("/rootfs"),
            helper_path: PathBuf::from("/definitely/not/here/agentkit-sandbox-vz-helper"),
            min_helper_version: (1, 0, 0),
            guest_vsock_port: crate::GUEST_AGENT_PORT,
        })
        .unwrap();
        let caps = b.probe().await.unwrap();
        assert!(!caps.hardware_virtualization);
        assert!(caps
            .notes
            .iter()
            .any(|n| n.contains("helper missing") || n.contains("missing")));
        let err = b.provision(&sample_spec()).await.unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::IsolationUnavailable);
    }

    fn sample_spec() -> VerifiedLaunchSpec {
        VerifiedLaunchSpec {
            sandbox_id: SandboxId::new("sbx-apple-test").unwrap(),
            tenant_id: TenantId::new("t-apple").unwrap(),
            instance_generation: InstanceGeneration(1),
            image_digest: ImageDigest::of_bytes(b"img"),
            trusted_kernel_digest: ImageDigest::of_bytes(b"img"),
            rootfs_digest: ArtifactDigest::of_bytes(b"rootfs"),
            spec: VerifiedSandboxSpec {
                tenant_id: TenantId::new("t-apple").unwrap(),
                owner: SandboxOwner {
                    principal_id: PrincipalId::new("p-apple").unwrap(),
                    agent_id: None,
                },
                lifecycle: Lifecycle::Ephemeral,
                image: ImageRef {
                    digest: ImageDigest::of_bytes(b"img"),
                    os: "linux".into(),
                    arch: agentkit_sandbox_contracts::host_arch().into(),
                },
                resources: EffectiveResources {
                    vcpus: 1,
                    memory_mib: 256,
                    writable_disk_mib: 64,
                    max_processes: 64,
                    command_timeout_sec: 30,
                    max_log_bytes: 1 << 20,
                },
                network: NetworkMode::None,
                idle_stop_sec: 0,
                workspace_root: GuestPath::parse("/workspace").unwrap(),
                policy_digest: PolicyDigest::of_bytes(b"policy"),
                policy_generation: PolicyGeneration(1),
            },
            boot_nonce_digest: ArtifactDigest::of_bytes(b"nonce"),
            boot_nonce_b64: "bm9uY2U=".into(),
            vsock_cid: 3,
            allow_guest_root: false,
        }
    }
}
