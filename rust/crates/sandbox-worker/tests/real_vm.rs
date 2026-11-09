//! REAL Linux VM end-to-end gate (evidence level L2, spec §15.1, §16 M1).
//!
//! Runs the complete path with **no fakes**: Firecracker/KVM/jailer backend,
//! the pinned `kernel.bin`/`rootfs.ext4` from `scripts/sandbox-image/registry`,
//! the real `agentkit-sandbox-guest` agent over vsock, real boot-nonce
//! channel association, real ext4 private disk.
//!
//! Deliberately NOT part of the default suite: requires hardware and admin
//! built images. Gate conditions (all must hold, otherwise the test reports
//! `NOT RUN` and passes vacuously — never pretending to have run):
//!   * build with `--features firecracker`
//!   * `AGENTKIT_REAL_VM=1` in the environment (explicit opt-in)
//!   * `/dev/kvm` readable+writable by the current uid
//!   * `AGENTKIT_SANDBOX_FC_BIN` (or `~/.local/bin/firecracker`) executable
//!   * `AGENTKIT_SANDBOX_JAILER_BIN` (or `~/.local/bin/jailer`) executable
//!   * `AGENTKIT_SANDBOX_IMAGE_DIR` (or `../../scripts/sandbox-image/registry`)
//!     containing `kernel.bin`, `rootfs.ext4` and their `.sha256` sidecars
//!
//! Run exactly:
//! ```text
//! AGENTKIT_REAL_VM=1 cargo test -p agentkit-sandbox-worker \
//!   --features firecracker --test real_vm -- --nocapture
//! ```

#![cfg(feature = "firecracker")]

use agentkit_sandbox_contracts::api::messages::{HostToWorker, WorkerReplyBody, WorkerToHost};
use agentkit_sandbox_contracts::api::{
    ControlEnvelope, ExecArgv, ExecEventKind, ExecRequest, VerifiedLaunchSpec,
};
use agentkit_sandbox_contracts::grant::{GuestEnvironment, WorkspaceGrant};
use agentkit_sandbox_contracts::ids::{
    ArtifactDigest, ExecutionId, InstanceGeneration, PrincipalId, RequestDigest, SandboxId,
    SessionId, TenantId, ToolCallId,
};
use agentkit_sandbox_contracts::path::GuestPath;
use agentkit_sandbox_contracts::policy::{NetworkMode, SandboxCapability};
use agentkit_sandbox_contracts::protocol::{FrameHeader, WireFrame};
use agentkit_sandbox_contracts::spec::{
    EffectiveResources, ImageRef, Lifecycle, SandboxOwner, VerifiedSandboxSpec,
};
use agentkit_sandbox_worker::backend::{BackendKind, make_firecracker_backend};
use agentkit_sandbox_worker::config::WorkerConfig;
use agentkit_sandbox_worker::guestlink::{DefaultGuestLinkFactory, GUEST_VSOCK_PORT};
use agentkit_sandbox_worker::registry::QuotaCeilings;
use agentkit_sandbox_worker::service::WorkerService;
use base64::Engine as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn env_or(key: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(key).map(PathBuf::from).unwrap_or(default)
}

fn skip_reason() -> Option<String> {
    if std::env::var("AGENTKIT_REAL_VM").ok().as_deref() != Some("1") {
        return Some("AGENTKIT_REAL_VM!=1 (real VM gate is explicit opt-in)".into());
    }
    // KVM usable by us?
    let kvm = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    if !kvm {
        return Some("/dev/kvm not readable+writable for this uid".into());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let fc = env_or(
        "AGENTKIT_SANDBOX_FC_BIN",
        PathBuf::from(format!("{home}/.local/bin/firecracker")),
    );
    let jailer = env_or(
        "AGENTKIT_SANDBOX_JAILER_BIN",
        PathBuf::from(format!("{home}/.local/bin/jailer")),
    );
    let img = env_or(
        "AGENTKIT_SANDBOX_IMAGE_DIR",
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sandbox-image/registry"),
    );
    for (what, p) in [
        ("firecracker binary", &fc),
        ("jailer binary", &jailer),
        ("kernel.bin", &img.join("kernel.bin")),
        ("rootfs.ext4", &img.join("rootfs.ext4")),
    ] {
        if !p.exists() {
            return Some(format!("{what} missing at {}", p.display()));
        }
    }
    None
}

fn digest_of_file(p: &Path) -> ArtifactDigest {
    ArtifactDigest::of_bytes(&std::fs::read(p).expect("image file readable"))
}

/// Verify a `.sha256` sidecar matches the file (registry integrity).
fn verify_sidecar(p: &Path) {
    let side = std::fs::read_to_string(format!("{}.sha256", p.display())).expect("sidecar");
    let want = side.split_whitespace().next().unwrap_or_default();
    let digest = digest_of_file(p);
    let got = match digest.as_str().split_once(':') {
        Some((_, hex)) => hex,
        None => panic!("digest shape"),
    };
    assert_eq!(got, want, "registry sidecar mismatch for {}", p.display());
}

fn now_ms_future() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000
}

async fn call(
    svc: &Arc<WorkerService>,
    request_id: &str,
    sequence: u64,
    msg: &HostToWorker,
) -> WorkerToHost {
    let frame = WireFrame {
        header: FrameHeader {
            protocol_version: agentkit_sandbox_contracts::PROTOCOL_VERSION,
            request_id: request_id.into(),
            sandbox_id: None,
            instance_generation: None,
            execution_id: None,
            operation: "real-vm".into(),
            sequence,
            deadline_ms: None,
            payload_len: 0,
            payload_digest: None,
        },
        body: serde_json::to_value(msg).unwrap(),
    };
    svc.handle(&frame).await
}

async fn ok(
    svc: &Arc<WorkerService>,
    request_id: &str,
    sequence: u64,
    msg: &HostToWorker,
) -> WorkerReplyBody {
    match call(svc, request_id, sequence, msg).await {
        WorkerToHost::Reply { outcome, .. } => match outcome {
            agentkit_sandbox_contracts::api::ReplyOutcomeShape::Ok { body } => body,
            agentkit_sandbox_contracts::api::ReplyOutcomeShape::Err { error } => {
                panic!("worker error: {error:?}")
            }
        },
        other => panic!("expected reply, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_firecracker_vm_exec_and_stop() {
    if let Some(reason) = skip_reason() {
        eprintln!("NOT RUN (real Linux VM gate): {reason}");
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let fc_bin = env_or(
        "AGENTKIT_SANDBOX_FC_BIN",
        PathBuf::from(format!("{home}/.local/bin/firecracker")),
    );
    let jailer_bin = env_or(
        "AGENTKIT_SANDBOX_JAILER_BIN",
        PathBuf::from(format!("{home}/.local/bin/jailer")),
    );
    let img = env_or(
        "AGENTKIT_SANDBOX_IMAGE_DIR",
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sandbox-image/registry"),
    );
    verify_sidecar(&img.join("kernel.bin"));
    verify_sidecar(&img.join("rootfs.ext4"));

    let tmp = tempfile::tempdir().unwrap();
    let backend = make_firecracker_backend(agentkit_sandbox_worker::backend::FirecrackerConfig {
        storage_root: tmp.path().join("storage"),
        kernel_path: img.join("kernel.bin"),
        rootfs_path: img.join("rootfs.ext4"),
        fc_bin: fc_bin.clone(),
        jailer_bin: jailer_bin.clone(),
        cgroup_root: std::env::var_os("AGENTKIT_SANDBOX_CGROUP_ROOT").map(PathBuf::from),
    })
    .expect("firecracker backend constructs with provided paths");

    // Exact pinned binary versions land in the evidence line at the end.
    let fc_ver = std::process::Command::new(&fc_bin)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let config = WorkerConfig {
        uds: tmp.path().join("worker.sock"),
        storage_root: tmp.path().join("storage"),
        peer_uids: vec![unsafe { libc::getuid() }],
        worker_id: "real-vm-worker".into(),
        revision: 1,
        install_fingerprint:
            "sha256:4444444444444444444444444444444444444444444444444444444444444444".into(),
        backend: BackendKind::Firecracker,
        guest_vsock_port: GUEST_VSOCK_PORT,
        kernel_path: Some(img.join("kernel.bin")),
        rootfs_path: Some(img.join("rootfs.ext4")),
        fc_bin: Some(fc_bin),
        jailer_bin: Some(jailer_bin),
        helper_path: None,
        cgroup_root: std::env::var_os("AGENTKIT_SANDBOX_CGROUP_ROOT").map(PathBuf::from),
        idle_sweep_sec: 0,
        ceilings: QuotaCeilings::default(),
        broker_enabled: false,
        broker_test_origins: Vec::new(),
    };
    let svc = WorkerService::new(
        config,
        backend,
        Arc::new(DefaultGuestLinkFactory),
        QuotaCeilings::default(),
    )
    .expect("worker service builds with real backend");

    let tenant = TenantId::new("realvm").unwrap();
    let sandbox = SandboxId::new("real-vm-1").unwrap();
    let generation = InstanceGeneration::new(1).unwrap();
    let policy_digest = agentkit_sandbox_contracts::ids::PolicyDigest::of_bytes(b"real-vm-policy");
    let policy_generation = agentkit_sandbox_contracts::ids::PolicyGeneration(1);

    // Handshake.
    let ack = call(
        &svc,
        "hello",
        0,
        &HostToWorker::Hello {
            host_id: "real-vm-test".into(),
            protocol_versions: vec![agentkit_sandbox_contracts::PROTOCOL_VERSION],
        },
    )
    .await;
    assert!(
        matches!(ack, WorkerToHost::HelloAck { .. }),
        "handshake must succeed: {ack:?}"
    );

    // REAL digests from the admin-built registry (sidecars verified above).
    let kernel_hex = {
        let d = digest_of_file(&img.join("kernel.bin"));
        d.as_str()
            .split_once(':')
            .map(|(_, h)| h.to_string())
            .unwrap()
    };
    let rootfs_digest = digest_of_file(&img.join("rootfs.ext4"));
    let rootfs_hex = rootfs_digest
        .as_str()
        .split_once(':')
        .map(|(_, h)| h.to_string())
        .unwrap();
    let kernel_img = agentkit_sandbox_contracts::ids::ImageDigest::from_hex(&kernel_hex).unwrap();
    let image_img = agentkit_sandbox_contracts::ids::ImageDigest::from_hex(&rootfs_hex).unwrap();

    // Random boot nonce (32 bytes from the CSPRNG).
    let mut boot_nonce = vec![0u8; 32];
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .expect("urandom")
            .read_exact(&mut boot_nonce)
            .expect("urandom read");
    }
    let boot_nonce_b64 = { base64::engine::general_purpose::STANDARD.encode(&boot_nonce) };

    let launch = VerifiedLaunchSpec {
        sandbox_id: sandbox.clone(),
        tenant_id: tenant.clone(),
        instance_generation: generation,
        image_digest: image_img.clone(),
        trusted_kernel_digest: kernel_img,
        rootfs_digest,
        spec: VerifiedSandboxSpec {
            tenant_id: tenant.clone(),
            owner: SandboxOwner {
                principal_id: PrincipalId::new("real-principal").unwrap(),
                agent_id: None,
            },
            lifecycle: Lifecycle::Ephemeral,
            image: ImageRef {
                digest: image_img,
                os: "linux".into(),
                arch: agentkit_sandbox_contracts::host_arch().into(),
            },
            resources: EffectiveResources {
                vcpus: 1,
                memory_mib: 512,
                writable_disk_mib: 1024,
                max_processes: 16,
                command_timeout_sec: 60,
                max_log_bytes: 1 << 20,
            },
            network: NetworkMode::None,
            idle_stop_sec: 0,
            workspace_root: GuestPath::parse("/workspace").unwrap(),
            policy_digest: policy_digest.clone(),
            policy_generation,
        },
        boot_nonce_digest: ArtifactDigest::of_bytes(&boot_nonce),
        boot_nonce_b64,
        vsock_cid: 3,
        allow_guest_root: false,
    };
    let envelope = ControlEnvelope {
        tenant_id: tenant.clone(),
        principal_id: PrincipalId::new("real-principal").unwrap(),
        sandbox_id: sandbox.clone(),
        instance_generation: generation,
        policy_generation,
        policy_digest,
        request_digest: RequestDigest::of_canonical_json(b"recorded-only"),
        capability: SandboxCapability::SandboxManage,
    };

    // Create: disk provisioning + boot + guest handshake (nonce association)
    // must all complete for a Ready reply.
    let body = ok(
        &svc,
        "create-real-1",
        1,
        &HostToWorker::CreateVm {
            envelope: envelope.clone(),
            launch,
        },
    )
    .await;
    assert!(
        matches!(body, WorkerReplyBody::Created { .. }),
        "real create must reach Ready: {body:?}"
    );

    // REAL exec: /bin/echo a marker, output verified from the guest over
    // real vsock. No fakes anywhere in this path.
    let mut req = ExecRequest {
        execution_id: ExecutionId::new("real-exec-1").unwrap(),
        tool_call_id: ToolCallId::new("tc-real").unwrap(),
        request_digest: RequestDigest::of_canonical_json(b"placeholder"),
        argv: ExecArgv::Direct {
            executable: "/bin/echo".into(),
            args: vec!["agentkit-real-vm-ok".into()],
        },
        cwd: GuestPath::parse("/workspace").unwrap(),
        environment: GuestEnvironment::new(),
        timeout_sec: 30,
        max_output_bytes: 1 << 20,
        max_processes: 16,
    };
    let d = agentkit_sandbox_worker::service::digest_of_request(&req).unwrap();
    req.request_digest = d.clone();
    let grant = WorkspaceGrant {
        tenant_id: tenant.clone(),
        principal_id: PrincipalId::new("real-principal").unwrap(),
        session_id: SessionId::new("sess-real").unwrap(),
        task_id: None,
        attempt_id: None,
        sandbox_id: sandbox.clone(),
        instance_generation: generation,
        policy_generation: agentkit_sandbox_contracts::ids::PolicyGeneration(1),
        policy_digest: envelope.policy_digest.clone(),
        capability: SandboxCapability::ProcessExec,
        writer_lease: None,
        expires_at_ms: now_ms_future(),
        intended_request_digest: d,
        tool_call_id: Some(ToolCallId::new("tc-real").unwrap()),
        permitted_paths: vec![GuestPath::parse("/workspace").unwrap()],
        exec_cwd: Some(GuestPath::parse("/workspace").unwrap()),
    };
    let body = ok(
        &svc,
        "exec-real-1",
        2,
        &HostToWorker::ExecStart {
            grant: grant.clone(),
            request: Box::new(req),
        },
    )
    .await;
    assert!(
        matches!(body, WorkerReplyBody::ExecStarted { .. }),
        "real exec must be accepted: {body:?}"
    );

    // Poll the durable event buffer for Exited with the expected stdout.
    let started = std::time::Instant::now();
    let (stdout_ok, exit_zero) = loop {
        let d2 = agentkit_sandbox_contracts::canonical::canonicalize(
            &serde_json::to_value(&(
                ExecutionId::new("real-exec-1")
                    .unwrap()
                    .as_str()
                    .to_string(),
                0u64,
            ))
            .unwrap(),
        );
        let digest2 = match d2 {
            Ok(bytes) => RequestDigest::of_canonical_json(&bytes),
            Err(_) => panic!("canonical events key"),
        };
        let g2 = WorkspaceGrant {
            intended_request_digest: digest2,
            ..grant.clone()
        };
        let body = ok(
            &svc,
            "events-real-poll",
            20,
            &HostToWorker::ExecEvents {
                grant: g2,
                execution_id: ExecutionId::new("real-exec-1").unwrap(),
                after_sequence: 0,
            },
        )
        .await;
        let events = match body {
            WorkerReplyBody::Events { events } => events,
            other => panic!("events reply: {other:?}"),
        };
        let exited = events.iter().find_map(|e| match &e.kind {
            ExecEventKind::Exited {
                exit_code,
                timed_out,
                ..
            } => Some((*exit_code, *timed_out)),
            _ => None,
        });
        if let Some((code, timed_out)) = exited {
            let out: Vec<u8> = events
                .iter()
                .filter_map(|e| match &e.kind {
                    ExecEventKind::StdoutChunk { bytes_b64, .. } => Some(bytes_b64),
                    _ => None,
                })
                .flat_map(|b| {
                    base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .unwrap_or_default()
                })
                .collect();
            break (
                out.windows(20).any(|w| w == b"agentkit-real-vm-ok"),
                code == Some(0) && !timed_out,
            );
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(120),
            "real exec must terminate within 120s; events: {events:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert!(stdout_ok, "guest stdout must carry the exec marker");
    assert!(exit_zero, "guest command must exit 0 without timeout");

    // Conclusive stop from real process observation.
    let body = ok(
        &svc,
        "stop-real-1",
        30,
        &HostToWorker::StopVm {
            envelope: envelope.clone(),
            force: false,
            reason: "real vm gate complete".into(),
        },
    )
    .await;
    match body {
        WorkerReplyBody::Stopped { evidence } => {
            assert!(evidence.conclusive, "real stop must be conclusive");
        }
        other => panic!("unexpected stop reply: {other:?}"),
    }

    // Clean destroy.
    let body = ok(
        &svc,
        "destroy-real-1",
        40,
        &HostToWorker::DestroyVm {
            envelope: envelope.clone(),
            force: true,
        },
    )
    .await;
    assert!(
        matches!(body, WorkerReplyBody::Destroyed { .. }),
        "destroy must yield a receipt: {body:?}"
    );

    // Durability sanity: the journal recorded the lifecycle.
    let records = agentkit_sandbox_worker::journal::Journal::replay(&svc.journal_path())
        .expect("journal replays");
    assert!(!records.is_empty(), "journal must record real lifecycle");

    eprintln!("REAL VM GATE PASSED: fc={fc_ver} kernel={kernel_hex} rootfs={rootfs_hex}");
}
