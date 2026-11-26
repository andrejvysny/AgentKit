//! The backend conformance suite against a real launcher and container
//! runtime (Docker 28+ or rootless Podman), run on the qualification VMs:
//!
//! ```text
//! AGENTKIT_SANDBOX_LAUNCHER_SOCKET=/run/agentkit-launcher/launcher.sock \
//! AGENTKIT_SANDBOX_SOCKET_DIR=/srv/agentkit/sockets \
//! AGENTKIT_SANDBOX_IMAGE_DIGEST=sha256:... \
//! cargo test -p agentkit-sandbox-container --test real_backend -- --ignored
//! ```
//!
//! Every case gets a fresh workspace id. A case leaves its machine running,
//! and the launcher keeps at most `max_containers` containers (default 8),
//! so the previous case's workspace is stopped and destroyed before the
//! next case starts; the last one at the end, also when a case fails. The
//! network cases run the
//! suite's `curl https://example.com/` without naming a proxy, so they
//! need an image whose `/etc/sandbox/environment` presets `HTTPS_PROXY`
//! to the shim (the version that ships `agentkit-sandboxd`); the
//! live-grant case reaches `https://example.com`.

mod support;

use std::sync::Arc;

use agentkit_testkit::conformance::sandbox_backend::{self, SandboxHarness};
use support::Sleeper;
use support::real::{self, with_cleanup};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn the_conformance_suite_passes_on_a_real_launcher() {
    let backend = real::backend(|_| {});
    let digest = real::image_digest();
    with_cleanup(backend, |backend, created| async move {
        sandbox_backend::run(|| {
            let (backend, created, digest) = (backend.clone(), created.clone(), digest.clone());
            async move {
                created.release(&backend).await;
                SandboxHarness {
                    backend: Arc::new(backend),
                    driver: Arc::new(Sleeper),
                    workspace_id: created.fresh("cf"),
                    image_digest: digest,
                }
            }
        })
        .await;
    })
    .await;
}
