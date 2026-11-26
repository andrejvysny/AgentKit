//! The backend conformance suite (`agentkit-testkit`) on the container
//! adapter, with the real supervisor running in this process behind a fake
//! launcher: real shell commands, real files, real egress proxy.
//!
//! `network_policy_applies_live` needs a granted connection to reach
//! `https://example.com`, so the default run plays that one case on the
//! testkit fake; the ignored test runs every case on the adapter and needs
//! internet access. The refusal case (`network_denied_without_grant_is_
//! drained`) never leaves the host and runs on the adapter in both.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_egress::EgressPorts;
use agentkit_testkit::FakeSandboxBackend;
use agentkit_testkit::conformance::sandbox_backend::{self, CASES, SandboxHarness, fake_harness};
use agentkit_workspaces::WorkspaceId;
use support::{DIGEST, Sleeper, backend_with, embedded};
use tempfile::TempDir;

/// Cases whose granted connection must reach the internet.
const NEEDS_INTERNET: &[&str] = &["network_policy_applies_live"];

/// A fresh adapter over a fresh launcher and supervisor data directory.
fn container_harness(dirs: &Mutex<Vec<TempDir>>, n: usize) -> SandboxHarness {
    let dir = tempfile::tempdir().unwrap();
    let starter = embedded::starter(dir.path().join("data"), DIGEST);
    let ports = EgressPorts::system();
    let (_launcher, backend) = backend_with(dir.path(), starter, |_| {}, ports);
    dirs.lock().unwrap().push(dir);
    SandboxHarness {
        backend: Arc::new(backend),
        driver: Arc::new(Sleeper),
        workspace_id: WorkspaceId::new(format!("ws-{n}")),
        image_digest: DIGEST.to_owned(),
    }
}

async fn run_suite(substitute: &[&str]) {
    for name in substitute {
        assert!(CASES.contains(name), "no conformance case is named {name}");
    }
    let dirs = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    // The suite builds one harness per case, in `CASES` order.
    sandbox_backend::run(|| {
        let n = next.fetch_add(1, Ordering::SeqCst);
        let harness = if substitute.contains(&CASES[n]) {
            fake_harness(FakeSandboxBackend::new())
        } else {
            container_harness(&dirs, n)
        };
        async move { harness }
    })
    .await;
    assert_eq!(next.load(Ordering::SeqCst), CASES.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_conformance_suite_passes_on_the_container_adapter() {
    run_suite(NEEDS_INTERNET).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "network: a granted connection reaches https://example.com through the egress proxy"]
async fn the_whole_suite_passes_with_internet_access() {
    run_suite(&[]).await;
}
