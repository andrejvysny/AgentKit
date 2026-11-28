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
//! cargo test -p agentkit-sandbox-microvm --test real_vm -- --ignored
//! ```
//!
//! Every case gets a fresh workspace; the previous case's is stopped and
//! destroyed first, so at most one VM runs. The network cases need the
//! root disk's `/etc/sandbox/environment` to preset the proxy, and the
//! live-grant case reaches `https://example.com`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentkit_egress::EgressPorts;
use agentkit_sandbox_container::{ContainerBackend, ContainerBackendConfig};
use agentkit_sandbox_microvm::{ImageFile, VmLauncher, VmLauncherConfig};
use agentkit_testkit::conformance::sandbox_backend::{self, SandboxDriver, SandboxHarness};
use agentkit_workspaces::{Generation, SandboxBackend, StopProof, WorkspaceId};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real: needs AGENTKIT_MICROVM_HELPER, AGENTKIT_MICROVM_KERNEL(_SHA256) and AGENTKIT_MICROVM_ROOT(_SHA256) on macOS"]
async fn the_conformance_suite_passes_over_real_microvms() {
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
