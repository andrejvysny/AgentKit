//! L1 parity/contract tests for the Virtualization.framework adapter using
//! the fake helper. These prove framing, gates, and lifecycle plumbing —
//! they are explicitly NOT isolation proofs (§15.1 L1). The real VZ matrix
//! is documented NOT RUN (docs/baseline/sandbox-macos-2026-11-08.md).

use std::path::{Path, PathBuf};

use agentkit_sandbox_contracts::api::{SandboxBackend, VerifiedLaunchSpec};
use agentkit_sandbox_contracts::error::SandboxErrorCode;
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ImageDigest, InstanceGeneration, PolicyDigest, PolicyGeneration, PrincipalId,
    SandboxId, TenantId,
};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::NetworkMode;
use agentkit_sandbox_contracts::spec::{
    EffectiveResources, ImageRef, Lifecycle, SandboxOwner, VerifiedSandboxSpec,
};

use agentkit_sandbox_worker_apple::backend::{AppleVzBackend, VzBackendConfig};

const FAKE_HELPER_EXE: &str = env!("CARGO_BIN_EXE_agentkit-sandbox-vz-fake-helper");

struct Fixture {
    tmp: tempfile::TempDir,
    kernel: PathBuf,
    rootfs: PathBuf,
    storage: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let kernel = tmp.path().join("kernel.bin");
        let rootfs = tmp.path().join("rootfs.ext4");
        std::fs::write(&kernel, b"fake-kernel-bytes").unwrap();
        std::fs::write(&rootfs, b"fake-rootfs-bytes").unwrap();
        let storage = tmp.path().join("storage");
        std::fs::create_dir_all(&storage).unwrap();
        Fixture {
            tmp,
            kernel,
            rootfs,
            storage,
        }
    }

    fn backend(&self, helper: &Path, kernel: Option<&Path>) -> AppleVzBackend {
        AppleVzBackend::new(VzBackendConfig {
            storage_root: self.storage.clone(),
            kernel_path: kernel.unwrap_or(&self.kernel).to_path_buf(),
            rootfs_path: self.rootfs.clone(),
            helper_path: helper.to_path_buf(),
            min_helper_version: (1, 0, 0),
            guest_vsock_port: 52,
        })
        .unwrap()
    }
}

fn spec() -> VerifiedLaunchSpec {
    let nonce = b"nonce-bytes-32-long-paddingg!";
    VerifiedLaunchSpec {
        sandbox_id: SandboxId::new("sbx-parity").unwrap(),
        tenant_id: TenantId::new("t-parity").unwrap(),
        instance_generation: InstanceGeneration(1),
        image_digest: ImageDigest::of_bytes(b"img"),
        trusted_kernel_digest: ImageDigest::of_bytes(b"fake-kernel-bytes"),
        rootfs_digest: ArtifactDigest::of_bytes(b"fake-rootfs-bytes"),
        spec: VerifiedSandboxSpec {
            tenant_id: TenantId::new("t-parity").unwrap(),
            owner: SandboxOwner {
                principal_id: PrincipalId::new("p-parity").unwrap(),
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
                writable_disk_mib: 16,
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
        boot_nonce_digest: ArtifactDigest::of_bytes(nonce),
        boot_nonce_b64: base64(nonce),
        vsock_cid: 3,
        allow_guest_root: false,
    }
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(char::from(T[(n >> 18) as usize & 63]));
        out.push(char::from(T[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(T[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(T[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

#[tokio::test]
async fn full_lifecycle_against_fake_helper() {
    let fx = Fixture::new();
    let backend = fx.backend(Path::new(FAKE_HELPER_EXE), None);

    let caps = backend.probe().await.unwrap();
    assert!(caps.hardware_virtualization, "fake helper reports support");
    assert_eq!(caps.backend, "apple-vz");

    let s = spec();
    let handle = backend.provision(&s).await.unwrap();
    assert_eq!(handle.backend, "apple-vz");
    // data disk is a zero sparse file at the worker-expected generation dir
    let dir = fx
        .storage
        .join("jailer")
        .join("ak-sbx-parity-g1")
        .join("root");
    assert!(dir.join("data.raw").exists());
    assert_eq!(
        std::fs::metadata(dir.join("data.raw")).unwrap().len(),
        16 * 1024 * 1024
    );

    let state = backend.launch(&handle).await.unwrap();
    assert_eq!(
        state.state,
        agentkit_sandbox_contracts::api::VmPhysState::Running
    );
    let pid = state.hypervisor_pid.expect("helper pid recorded");
    let identity = state.process_identity.clone().expect("identity");
    assert_eq!(identity.pid, pid);
    assert!(identity.start_time_ms > 0);
    assert!(
        dir.join("vsock.uds").exists(),
        "relay socket at worker-expected path"
    );
    assert!(dir.join("booted.marker").exists());
    // manifest must carry the boot nonce and the fresh-generation format flag
    let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap();
    assert!(manifest.contains(&format!("agentkit.boot_nonce={}", s.boot_nonce_b64)));
    assert!(manifest.contains("agentkit.format_data=1"));
    // no network anywhere in the manifest
    assert!(
        !manifest.contains("net"),
        "manifest must not mention networking"
    );

    let observed = backend.inspect(&handle).await.unwrap();
    assert_eq!(
        observed.state,
        agentkit_sandbox_contracts::api::VmPhysState::Running
    );

    let evidence = backend.terminate(&handle).await.unwrap();
    assert_eq!(evidence.method, "helper_stop");
    assert!(evidence.conclusive, "reaping our own child is conclusive");
    assert_eq!(evidence.identity.as_ref().unwrap().pid, pid);

    let after = backend.inspect(&handle).await.unwrap();
    assert_eq!(
        after.state,
        agentkit_sandbox_contracts::api::VmPhysState::Unknown
    );

    // second launch on the same generation re-provisions fresh: marker keeps
    // the data disk from being reformatted (persistent-safety).
    let handle2 = backend.provision(&s).await;
    assert!(handle2.is_err(), "generation dir already exists");
}

#[tokio::test]
async fn missing_helper_fails_closed() {
    let fx = Fixture::new();
    let backend = fx.backend(Path::new("/nonexistent/agentkit-sandbox-vz-helper"), None);
    let caps = backend.probe().await.unwrap();
    assert!(!caps.hardware_virtualization);
    let err = backend.provision(&spec()).await.unwrap_err();
    assert_eq!(err.code, SandboxErrorCode::IsolationUnavailable);
}

#[tokio::test]
async fn kernel_or_rootfs_digest_mismatch_refused_before_helper_spawn() {
    let fx = Fixture::new();
    // Pin a wrong kernel digest by swapping the file content post-verify-pin:
    let evil = fx.tmp.path().join("evil-kernel");
    std::fs::write(&evil, b"tampered-kernel").unwrap();
    let backend = fx.backend(Path::new(FAKE_HELPER_EXE), Some(&evil));
    let err = backend.provision(&spec()).await.unwrap_err();
    assert!(
        matches!(
            err.code,
            SandboxErrorCode::ImageInvalid | SandboxErrorCode::IsolationUnavailable
        ),
        "{err:?}"
    );
    let msg = err.message.to_lowercase();
    assert!(msg.contains("kernel digest mismatch"), "{err}");
}

#[tokio::test]
async fn helper_death_during_launch_is_error_not_hang() {
    let fx = Fixture::new();
    // Helper that boots on missing images: point rootfs at a good file but
    // delete it after provision, before launch.
    let backend = fx.backend(Path::new(FAKE_HELPER_EXE), None);
    let s = spec();
    let handle = backend.provision(&s).await.unwrap();
    std::fs::write(&fx.rootfs, b"fake-rootfs-bytes").unwrap(); // still pinned ok
                                                               // Sabotage the manifest to a deleted data disk: helper exits pre-relay.
    let dir = fx
        .storage
        .join("jailer")
        .join("ak-sbx-parity-g1")
        .join("root");
    std::fs::remove_file(dir.join("data.raw")).unwrap();
    eprintln!(
        "DBG dir entries: {:?}",
        std::fs::read_dir(&dir).map(|r| r
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect::<Vec<_>>())
    );
    let err = match backend.launch(&handle).await {
        Ok(s) => {
            eprintln!("DBG unexpected Ok: {:?}", s);
            eprintln!(
                "DBG manifest: {:?}",
                std::fs::read_to_string(dir.join("manifest.json"))
            );
            eprintln!(
                "DBG vm.log: {:?}",
                std::fs::read_to_string(dir.join("vm.log"))
            );
            eprintln!(
                "DBG dir2: {:?}",
                std::fs::read_dir(&dir).map(|r| r
                    .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                    .collect::<Vec<_>>())
            );
            panic!("launch should have failed");
        }
        Err(e) => e,
    };
    eprintln!(
        "DBG manifest: {}",
        std::fs::read_to_string(dir.join("manifest.json")).unwrap_or_default()
    );
    let log = std::fs::read_to_string(dir.join("vm.log")).unwrap_or_default();
    assert!(
        log.contains("manifest path missing"),
        "helper should have rejected the missing data disk; vm.log: {log}"
    );
    assert!(
        err.message.contains("vsock relay") || err.message.contains("helper exited"),
        "{err}"
    );
}

/// L3 real Virtualization.framework gate (macOS, Apple Silicon). Opt-in and
/// honestly skipped: without `AGENTKIT_REAL_VZ=1`, a non-macOS host, or the
/// signed helper + images, this prints NOT RUN — a skip is never a pass.
/// Recipe: docs/baseline/sandbox-macos-2026-11-08.md.
#[tokio::test]
#[ignore = "requires a real macOS host with a signed VZ helper (L3)"]
async fn real_vz_boot_and_termination() {
    if std::env::var("AGENTKIT_REAL_VZ").ok().as_deref() != Some("1") {
        eprintln!("NOT RUN: AGENTKIT_REAL_VZ!=1 (real VZ gate is explicit opt-in)");
        return;
    }
    if !cfg!(target_os = "macos") {
        eprintln!("NOT RUN: Virtualization.framework gate requires macOS");
        return;
    }
    let helper = PathBuf::from(
        std::env::var("AGENTKIT_VZ_HELPER_BIN")
            .expect("AGENTKIT_VZ_HELPER_BIN: signed helper path"),
    );
    let img = PathBuf::from(std::env::var("AGENTKIT_VZ_IMAGE_DIR").expect("AGENTKIT_VZ_IMAGE_DIR"));
    let fx = Fixture::new();
    std::fs::write(&fx.kernel, std::fs::read(img.join("kernel.bin")).unwrap()).unwrap();
    std::fs::write(&fx.rootfs, std::fs::read(img.join("rootfs.ext4")).unwrap()).unwrap();
    let backend = fx.backend(&helper, None);
    let caps = backend.probe().await.unwrap();
    assert!(
        caps.hardware_virtualization,
        "signed helper must report support: {:?}",
        caps.notes
    );
    let s = spec();
    let handle = backend.provision(&s).await.expect("provision on real VZ");
    let state = backend.launch(&handle).await.expect("real boot");
    assert_eq!(
        state.state,
        agentkit_sandbox_contracts::api::VmPhysState::Running
    );
    let ev = backend.terminate(&handle).await.unwrap();
    assert!(
        ev.conclusive,
        "real termination must be conclusively observed"
    );
}
