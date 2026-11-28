//! The backend conformance suite (`agentkit-testkit`) on the unchanged
//! `ContainerBackend` over a real `VmLauncher` and real helpers (ADR 0040,
//! section 6). macOS only; the helper must be ad hoc signed with its
//! entitlement first. `cargo test` relinks `target/debug/
//! agentkit-sandbox-vmhelper` (dropping the signature), so sign a copy:
//!
//! ```text
//! cp target/debug/agentkit-sandbox-vmhelper /abs/ && \
//!   crates/sandbox-microvm/scripts/sign-helper.sh /abs/agentkit-sandbox-vmhelper
//! AGENTKIT_MICROVM_HELPER=/abs/agentkit-sandbox-vmhelper \
//! AGENTKIT_MICROVM_KERNEL=/abs/Image AGENTKIT_MICROVM_KERNEL_SHA256=<hex> \
//! AGENTKIT_MICROVM_ROOT=/abs/root.ext4 AGENTKIT_MICROVM_ROOT_SHA256=<hex> \
//! cargo test -p agentkit-sandbox-microvm --test real_vm -- --ignored --test-threads=1
//! ```
//!
//! Every case gets a fresh workspace; the previous case's is stopped and
//! destroyed first, so at most one VM runs (one test at a time, hence
//! `--test-threads=1`). The network cases need the root disk's
//! `/etc/sandbox/environment` to preset the proxy, and the live-grant case
//! reaches `https://example.com`. The guest case checks what the workload
//! itself sees of its VM (ADR 0040, section 4).

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentkit_egress::EgressPorts;
use agentkit_sandbox_container::{ContainerBackend, ContainerBackendConfig};
use agentkit_sandbox_microvm::{ImageFile, VmLauncher, VmLauncherConfig};
use agentkit_testkit::conformance::sandbox_backend::{self, SandboxDriver, SandboxHarness};
use agentkit_workspaces::{
    Generation, ListEntries, Listing, MachineHandle, MachineSpec, NetworkPolicy, OperationId,
    OperationKind, OperationRequest, OperationState, OutputLimit, PathRoot, RelPath,
    ResourceLimits, SandboxBackend, Sha256, StopProof, WorkspaceId,
};
use async_trait::async_trait;

/// Highest generation a case starts; one stop of it covers every lower one.
const GENERATIONS_USED: u64 = 4;

/// Lets real time pass.
struct Sleeper;

#[async_trait]
impl SandboxDriver for Sleeper {
    async fn elapse(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the real microVM test"))
}

fn image(path: &str, sha256: &str) -> ImageFile {
    ImageFile {
        path: PathBuf::from(var(path)),
        sha256: var(sha256),
    }
}

/// Workspaces the suite started, released between cases.
#[derive(Clone, Default)]
struct Used(Arc<Mutex<Vec<WorkspaceId>>>);

impl Used {
    fn fresh(&self, n: usize) -> WorkspaceId {
        let ws = WorkspaceId::new(format!("vm-{n}"));
        self.0.lock().unwrap().push(ws.clone());
        ws
    }

    /// Stop and destroy every workspace started so far.
    async fn release(&self, backend: &ContainerBackend) {
        let used = std::mem::take(&mut *self.0.lock().unwrap());
        for ws in used {
            let proof = backend
                .stop_machine(&ws, Generation(GENERATIONS_USED))
                .await
                .unwrap_or_else(|error| panic!("stop {ws}: {error}"));
            assert_eq!(proof, StopProof::Stopped, "stop {ws}");
            backend
                .destroy_storage(&ws)
                .await
                .unwrap_or_else(|error| panic!("destroy {ws}: {error}"));
        }
    }
}

/// A real `VmLauncher` from the environment and the unchanged adapter over
/// it; `_dirs` (data root, socket root) must outlive the launcher.
struct Rig {
    launcher: VmLauncher,
    backend: ContainerBackend,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn start_rig() -> Rig {
    let scratch = tempfile::tempdir().unwrap();
    let sockets = tempfile::Builder::new()
        .prefix("aksvm")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap();
    let config = VmLauncherConfig::new(
        scratch.path().join("data"),
        sockets.path().to_path_buf(),
        PathBuf::from(var("AGENTKIT_MICROVM_HELPER")),
        image("AGENTKIT_MICROVM_KERNEL", "AGENTKIT_MICROVM_KERNEL_SHA256"),
        image("AGENTKIT_MICROVM_ROOT", "AGENTKIT_MICROVM_ROOT_SHA256"),
    );
    // The quota covers both disks, as usage does.
    let disk_bytes = config.workspace_disk_bytes + config.home_disk_bytes;
    let launcher = VmLauncher::start(config)
        .await
        .expect("the launcher starts");
    let mut adapter = ContainerBackendConfig::new(
        launcher.listen_socket().to_path_buf(),
        launcher.socket_root().to_path_buf(),
    );
    adapter.image_digest = Some(launcher.image_digest().to_owned());
    adapter.microvm_disk_bytes = Some(disk_bytes);
    let backend = ContainerBackend::new(adapter, EgressPorts::system()).unwrap();
    assert_eq!(backend.describe().kind, "microvm");
    Rig {
        launcher,
        backend,
        _dirs: (scratch, sockets),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real: needs AGENTKIT_MICROVM_HELPER, AGENTKIT_MICROVM_KERNEL(_SHA256) and AGENTKIT_MICROVM_ROOT(_SHA256) on macOS"]
async fn the_conformance_suite_passes_over_real_microvms() {
    let Rig {
        launcher,
        backend,
        _dirs,
    } = start_rig().await;
    let (used, next) = (Used::default(), AtomicUsize::new(0));
    let digest = launcher.image_digest().to_owned();
    sandbox_backend::run(|| {
        let n = next.fetch_add(1, Ordering::SeqCst);
        let (backend, used, digest) = (backend.clone(), used.clone(), digest.clone());
        async move {
            used.release(&backend).await;
            SandboxHarness {
                workspace_id: used.fresh(n),
                backend: Arc::new(backend),
                driver: Arc::new(Sleeper),
                image_digest: digest,
            }
        }
    })
    .await;
    used.release(&backend).await;
    launcher.shutdown().await.expect("every helper stopped");
}

/// What the workload reads of its own VM, as uid 1000 through the backend:
/// the command line copies (the channel token), the boot log, its
/// credentials, new disks, io_uring, the `/dev` mount and the
/// `fs.protected_*` settings. Everything is collected first and checked
/// once the VM is stopped and the launcher shut down, so a failed check
/// leaves no VM behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real: needs AGENTKIT_MICROVM_HELPER, AGENTKIT_MICROVM_KERNEL(_SHA256) and AGENTKIT_MICROVM_ROOT(_SHA256) on macOS"]
async fn the_workload_reads_no_token_log_or_privilege_of_its_vm() {
    let Rig {
        launcher,
        backend,
        _dirs,
    } = start_rig().await;
    let ws = WorkspaceId::new("vm-guest");
    let digest = launcher.image_digest().to_owned();
    let probed = tokio::spawn(probe_guest(backend.clone(), ws.clone(), digest)).await;
    let stopped = backend.stop_machine(&ws, Generation(1)).await;
    let destroyed = backend.destroy_storage(&ws).await;
    launcher.shutdown().await.expect("every helper stopped");
    let seen = probed.unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()));
    assert_eq!(stopped.expect("stop"), StopProof::Stopped);
    destroyed.expect("destroy");
    check_guest(&seen);
}

/// Shell text the workload runs, by name; each prints what `check_guest`
/// expects. `fresh` runs first, on the new disks.
const GUEST_PROBES: [(&str, &str); 8] = [
    ("fresh", "ls -A /workspace; echo --; ls -A /home/agent"),
    (
        "cmdline",
        r#"if c=$(cat /proc/cmdline 2>/dev/null); then echo "readable ${#c}"; else echo unreadable; fi"#,
    ),
    (
        "bootargs",
        r#"for f in /sys/firmware/devicetree/base/chosen/bootargs /sys/firmware/fdt; do
             if [ ! -e "$f" ]; then echo absent; elif cat "$f" >/dev/null 2>&1; then echo "readable $f";
             else echo unreadable; fi; done"#,
    ),
    (
        "log",
        "command -v dmesg >/dev/null || echo no-dmesg; \
         dmesg >/dev/null 2>&1 && echo dmesg-read || echo dmesg-refused; \
         head -c1 /dev/kmsg >/dev/null 2>&1 && echo kmsg-read || echo kmsg-refused; \
         stat -c %a /dev/kmsg",
    ),
    (
        "status",
        "grep -E '^(Uid|Gid|CapPrm|CapEff|CapBnd|NoNewPrivs):' /proc/self/status",
    ),
    (
        "io_uring",
        r#"cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo absent
python3 -I -c 'import ctypes, errno
libc = ctypes.CDLL(None, use_errno=True)
fd = libc.syscall(425, 1, ctypes.create_string_buffer(120))
print(fd, errno.errorcode.get(ctypes.get_errno(), "none"))'"#,
    ),
    ("dev", "grep ' /dev ' /proc/mounts"),
    (
        "protected",
        "cd /proc/sys/fs && cat protected_symlinks protected_hardlinks protected_fifos protected_regular",
    ),
];

/// What the probes saw: both new trees listed through the port before any
/// command ran, then each probe's receipt and output.
struct GuestView {
    workspace: Listing,
    home: Listing,
    probes: Vec<(&'static str, OperationState, String)>,
}

async fn probe_guest(backend: ContainerBackend, ws: WorkspaceId, digest: String) -> GuestView {
    let spec = MachineSpec {
        workspace_id: ws,
        generation: Generation(1),
        image_digest: digest,
        limits: ResourceLimits::DEFAULT,
        network: NetworkPolicy::default(),
    };
    let m = backend.ensure_machine(&spec).await.expect("the VM starts");
    let list = |root| ListEntries {
        root,
        path: RelPath::root(),
        depth: 1,
        max_entries: 16,
    };
    let workspace = backend.list(&m, &list(PathRoot::Workspace)).await;
    let home = backend.list(&m, &list(PathRoot::Home)).await;
    let mut probes = Vec::new();
    for (name, command) in GUEST_PROBES {
        let (state, output) = run_probe(&backend, &m, name, command).await;
        probes.push((name, state, output));
    }
    GuestView {
        workspace: workspace.expect("list /workspace"),
        home: home.expect("list /home/agent"),
        probes,
    }
}

/// Dispatch `command` as operation `op-<name>` and read it to its receipt.
async fn run_probe(
    backend: &ContainerBackend,
    m: &MachineHandle,
    name: &str,
    command: &str,
) -> (OperationState, String) {
    let op = OperationRequest {
        id: OperationId::new(format!("op-{name}")),
        workspace_id: m.workspace_id.clone(),
        generation: m.generation,
        args_sha256: Sha256::of(command.as_bytes()),
        kind: OperationKind::Exec {
            command: command.to_owned(),
            cwd: RelPath::root(),
            timeout_ms: 60_000,
            output_limit: OutputLimit::LOG,
        },
    };
    backend.dispatch(m, &op).await.expect("dispatch");
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut seq, mut output) = (0, Vec::new());
    loop {
        let seen = backend.observe(m, &op.id, seq).await.expect("observe");
        seq = seen.next_seq;
        output.extend_from_slice(&seen.output);
        if seen.state.is_terminal() && seen.eof {
            return (seen.state, String::from_utf8_lossy(&output).into_owned());
        }
        assert!(
            Instant::now() < deadline,
            "{name} has no receipt after 120 s"
        );
        if seen.output.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn check_guest(seen: &GuestView) {
    assert!(seen.workspace.entries.is_empty(), "{:?}", seen.workspace);
    assert!(seen.home.entries.is_empty(), "{:?}", seen.home);
    for (name, state, output) in &seen.probes {
        assert_eq!(
            *state,
            OperationState::Succeeded { exit: 0 },
            "{name}: {output}"
        );
        let lines: Vec<&str> = output.lines().collect();
        match *name {
            // The command's own log directory is the only entry.
            "fresh" => assert_eq!(lines, [".sandbox", "--"], "{name}"),
            "cmdline" => assert!(
                matches!(lines[..], ["unreadable"] | ["readable 0"]),
                "{name}: {output}"
            ),
            "bootargs" => assert!(
                lines.iter().all(|l| matches!(*l, "absent" | "unreadable")),
                "{name}: {output}"
            ),
            "log" => assert_eq!(lines, ["dmesg-refused", "kmsg-refused", "600"], "{name}"),
            "status" => check_status(&lines),
            "io_uring" => assert!(
                matches!(lines[..], ["absent", "-1 ENOSYS"] | ["2", "-1 EPERM"]),
                "{name}: {output}"
            ),
            "dev" => {
                let options = lines
                    .first()
                    .and_then(|line| line.split(' ').nth(3))
                    .unwrap_or("");
                let options: Vec<&str> = options.split(',').collect();
                assert!(
                    options.contains(&"nosuid") && options.contains(&"noexec"),
                    "{name}: {output}"
                );
            }
            "protected" => assert_eq!(lines, ["1", "1", "2", "2"], "{name}"),
            other => panic!("no check for probe {other}"),
        }
    }
}

/// uid and gid 1000 throughout, no capability left, no_new_privs set.
fn check_status(lines: &[&str]) {
    let field = |key: &str| {
        lines
            .iter()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
            .unwrap_or_else(|| panic!("no {key} in {lines:?}"))
            .split_whitespace()
            .collect::<Vec<_>>()
    };
    for key in ["Uid", "Gid"] {
        assert_eq!(field(key), ["1000"; 4], "{key}");
    }
    for key in ["CapPrm", "CapEff", "CapBnd"] {
        let mask = field(key);
        assert!(
            mask.len() == 1 && mask[0].bytes().all(|b| b == b'0'),
            "{key}: {mask:?}"
        );
    }
    assert_eq!(field("NoNewPrivs"), ["1"]);
}
