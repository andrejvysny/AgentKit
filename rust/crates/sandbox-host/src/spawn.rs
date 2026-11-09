//! Launch the `agentkit-sandbox-worker` binary and hand out a connected
//! [`WorkerClient`].
//!
//! Child exit => every pending request resolves through the standard
//! socket-death path: mutations as `sandbox_execution_uncertain` /
//! `sandbox_termination_unverified`, idempotent reads retried once (and if
//! the worker is gone, `sandbox_unavailable`). Nothing is ever auto-replayed
//! for a mutation (S07/S14).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::net::UnixStream;

use crate::client::{ClientBinding, HostAdmission, HostError, WorkerClient, WorkerClientConfig};

/// uid of the current process (the peer uid to advertise to the worker).
#[must_use]
pub fn current_uid() -> u32 {
    // SAFETY: getuid is always callable.
    unsafe { libc::getuid() }
}

/// Everything the admin CLI needs to bring the worker up (paths arrive only
/// from trusted host configuration, S20).
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Path of the `agentkit-sandbox-worker` executable.
    pub worker_bin: PathBuf,
    /// Worker UDS path (worker binds it; host connects + peer-auths).
    pub uds: PathBuf,
    /// Worker private storage root (journal + per-generation disks).
    pub storage_root: PathBuf,
    /// uid the worker must accept on `SO_PEERCRED` (usually this process's).
    pub peer_uid: u32,
    /// uid this client requires the worker to run as.
    pub expected_worker_uid: u32,
    pub worker_id: String,
    pub install_fingerprint: String,
    /// `fake` or `firecracker` (as the worker CLI defines).
    pub backend: String,
    /// Additional admin flags (e.g. firecracker paths).
    pub extra_args: Vec<String>,
    pub startup_timeout: Duration,
}

impl SpawnConfig {
    /// Flags exactly as `agentkit_sandbox_worker::config::WorkerConfig::
    /// from_args` consumes them.
    #[must_use]
    pub fn worker_args(&self) -> Vec<String> {
        let mut args = vec![
            "--uds".to_string(),
            self.uds.display().to_string(),
            "--storage-root".to_string(),
            self.storage_root.display().to_string(),
            "--peer-uid".to_string(),
            self.peer_uid.to_string(),
            "--worker-id".to_string(),
            self.worker_id.clone(),
            "--install-fingerprint".to_string(),
            self.install_fingerprint.clone(),
            "--backend".to_string(),
            self.backend.clone(),
        ];
        args.extend(self.extra_args.iter().cloned());
        args
    }
}

/// A running worker plus a client bound to one sandbox identity.
pub struct SpawnedWorker {
    child: std::sync::Arc<Mutex<std::process::Child>>,
    client: std::sync::Arc<WorkerClient>,
}

impl SpawnedWorker {
    #[must_use]
    pub fn client(&self) -> std::sync::Arc<WorkerClient> {
        std::sync::Arc::clone(&self.client)
    }

    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        Some(self.child.lock().expect("child lock").id())
    }

    /// SIGKILL the worker (crash-path tests; graceful drain is SIGTERM).
    pub fn kill(&self) -> std::io::Result<()> {
        self.child.lock().expect("child lock").kill()
    }

    /// Reap the child once it exits (blocking wait on a helper thread).
    pub async fn wait(&self) -> std::io::Result<std::process::ExitStatus> {
        let child = std::sync::Arc::clone(&self.child);
        tokio::task::spawn_blocking(move || child.lock().expect("child lock").wait())
            .await
            .map_err(|e| std::io::Error::other(format!("wait join: {e}")))?
    }
}

/// Spawn the worker binary, wait for its socket, and return a client that
/// has completed Hello/HelloAck.
pub async fn spawn_worker(
    spawn: SpawnConfig,
    client_config: WorkerClientConfig,
    binding: ClientBinding,
    admission: HostAdmission,
) -> Result<SpawnedWorker, HostError> {
    let child = Command::new(&spawn.worker_bin)
        .args(spawn.worker_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            HostError::WorkerExited(format!("spawn {}: {e}", spawn.worker_bin.display()))
        })?;
    let mut child = Some(child);
    let deadline = Instant::now() + spawn.startup_timeout;
    loop {
        // Fail fast if the worker refused to start (bad flags, uid, …).
        if let Some(ch) = child.as_mut()
            && let Ok(Some(status)) = ch.try_wait()
        {
            return Err(HostError::WorkerExited(format!(
                "worker exited during startup: {status}"
            )));
        }
        if UnixStream::connect(&spawn.uds).await.is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            if let Some(mut ch) = child.take() {
                let _ = ch.kill();
                let _ = ch.wait();
            }
            return Err(HostError::Connect(format!(
                "worker socket {} did not come up within {:?}",
                spawn.uds.display(),
                spawn.startup_timeout
            )));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Drop our probe connection implicitly (temp value). The real client
    // connects per its own lifecycle.
    let client = WorkerClient::new(client_config, binding, admission);
    // Eager handshake so "handed out" means "associated".
    client.connect().await?;
    Ok(SpawnedWorker {
        child: std::sync::Arc::new(Mutex::new(child.expect("worker alive after socket poll"))),
        client,
    })
}
