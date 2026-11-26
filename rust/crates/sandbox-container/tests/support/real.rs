//! Real-backend helpers: the adapter over the launcher named by the
//! environment, fresh workspace ids, commands run to their receipt, and
//! a cleanup that runs even when a test panics.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentkit_egress::EgressPorts;
use agentkit_sandbox_container::{ContainerBackend, ContainerBackendConfig};
use agentkit_workspaces::{
    Denial, Generation, MachineHandle, MachineSpec, NetworkPolicy, OperationId, OperationKind,
    OperationRequest, OperationState, OutputLimit, RelPath, ResourceLimits, SandboxBackend, Sha256,
    StopProof, WorkspaceId,
};

pub const LAUNCHER_SOCKET: &str = "AGENTKIT_SANDBOX_LAUNCHER_SOCKET";
pub const SOCKET_DIR: &str = "AGENTKIT_SANDBOX_SOCKET_DIR";
pub const IMAGE_DIGEST: &str = "AGENTKIT_SANDBOX_IMAGE_DIGEST";
/// Highest generation any test starts; cleanup's one stop of it covers
/// every lower one (the launcher stops all generations at or below).
pub const GENERATIONS_USED: u64 = 4;

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for a real-backend test"))
}

/// The image digest the launcher runs.
pub fn image_digest() -> String {
    var(IMAGE_DIGEST)
}

/// The launcher socket path.
pub fn launcher_socket() -> PathBuf {
    PathBuf::from(var(LAUNCHER_SOCKET))
}

/// The adapter over the environment's launcher, tuned by `tune`.
pub fn backend(tune: impl FnOnce(&mut ContainerBackendConfig)) -> ContainerBackend {
    let mut config = ContainerBackendConfig::new(launcher_socket(), PathBuf::from(var(SOCKET_DIR)));
    tune(&mut config);
    ContainerBackend::new(config, EgressPorts::system()).expect("a valid backend config")
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A workspace id no earlier run used: `rb-<tag>-<epoch ms>-<n>`.
pub fn fresh_workspace(tag: &str) -> WorkspaceId {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    WorkspaceId::new(format!("rb-{tag}-{millis}-{n}"))
}

/// Workspaces a test created, destroyed at its end.
#[derive(Clone, Default)]
pub struct Created(Arc<Mutex<Vec<WorkspaceId>>>);

impl Created {
    pub fn fresh(&self, tag: &str) -> WorkspaceId {
        let ws = fresh_workspace(tag);
        self.0.lock().unwrap().push(ws.clone());
        ws
    }

    /// Stop and destroy every workspace created so far and forget it.
    /// Panics on the first failure. A suite that starts many workspaces
    /// calls it between cases: the launcher keeps at most
    /// `max_containers` containers (default 8, any state, all workspaces).
    pub async fn release(&self, backend: &ContainerBackend) {
        for ws in self.take() {
            if let Err(problem) = stop_and_destroy(backend, &ws).await {
                panic!("releasing {ws} before the next case: {problem}");
            }
        }
    }

    /// Stop and destroy every workspace created so far and forget it.
    /// Best effort: failures are printed, not raised.
    pub async fn destroy(&self, backend: &ContainerBackend) {
        for ws in self.take() {
            if let Err(problem) = stop_and_destroy(backend, &ws).await {
                eprintln!("cleanup: {problem}");
            }
        }
    }

    fn take(&self) -> Vec<WorkspaceId> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// Stop every generation a test may have started (one stop of the highest
/// covers the lower ones), then destroy the volumes.
async fn stop_and_destroy(backend: &ContainerBackend, ws: &WorkspaceId) -> Result<(), String> {
    let generation = Generation(GENERATIONS_USED);
    match backend.stop_machine(ws, generation).await {
        Ok(StopProof::Stopped) => {}
        Ok(StopProof::Unproven { reason }) => {
            return Err(format!(
                "stop {ws} through {generation}: unproven: {reason}"
            ));
        }
        Err(error) => return Err(format!("stop {ws} through {generation}: {error}")),
    }
    backend
        .destroy_storage(ws)
        .await
        .map_err(|error| format!("destroy {ws}: {error}"))
}

/// Run `body`, then destroy what it created, then re-raise its panic.
pub async fn with_cleanup<F, Fut>(backend: ContainerBackend, body: F)
where
    F: FnOnce(ContainerBackend, Created) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let created = Created::default();
    let outcome = tokio::spawn(body(backend.clone(), created.clone())).await;
    created.destroy(&backend).await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error.into_panic());
    }
}

/// The spec of generation 1 of `ws` with `grants`.
pub fn spec(ws: &WorkspaceId, grants: &[(&str, u16)]) -> MachineSpec {
    let grants = grants
        .iter()
        .map(|(host, port)| agentkit_workspaces::HostPort::new(host, *port).expect("valid grant"))
        .collect();
    MachineSpec {
        workspace_id: ws.clone(),
        generation: Generation(1),
        image_digest: image_digest(),
        limits: ResourceLimits::DEFAULT,
        network: NetworkPolicy { grants },
    }
}

pub fn exec(m: &MachineHandle, id: &str, command: &str, timeout_ms: u64) -> OperationRequest {
    OperationRequest {
        id: OperationId::new(id),
        workspace_id: m.workspace_id.clone(),
        generation: m.generation,
        args_sha256: Sha256::of(command.as_bytes()),
        kind: OperationKind::Exec {
            command: command.to_owned(),
            cwd: RelPath::root(),
            timeout_ms,
            output_limit: OutputLimit::LOG,
        },
    }
}

/// A receipt and the whole output.
#[derive(Debug)]
pub struct Done {
    pub state: OperationState,
    pub output: String,
}

impl Done {
    pub fn succeeded(&self) -> bool {
        self.state == OperationState::Succeeded { exit: 0 }
    }
}

/// Poll `id` until its receipt and every output byte are read.
pub async fn finish(backend: &ContainerBackend, m: &MachineHandle, id: &OperationId) -> Done {
    let deadline = Instant::now() + Duration::from_secs(300);
    let (mut seq, mut output) = (0, Vec::new());
    loop {
        let seen = backend.observe(m, id, seq).await.expect("observe");
        seq = seen.next_seq;
        output.extend_from_slice(&seen.output);
        if seen.state.is_terminal() && seen.eof {
            let output = String::from_utf8_lossy(&output).into_owned();
            return Done {
                state: seen.state,
                output,
            };
        }
        assert!(Instant::now() < deadline, "{id} has no receipt after 300 s");
        if seen.output.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Dispatch `command` as `id` and wait for its receipt.
pub async fn run(backend: &ContainerBackend, m: &MachineHandle, id: &str, command: &str) -> Done {
    let op = exec(m, id, command, 120_000);
    backend.dispatch(m, &op).await.expect("dispatch");
    finish(backend, m, &op.id).await
}

/// Every denial recorded so far.
pub async fn denials(backend: &ContainerBackend, m: &MachineHandle) -> Vec<Denial> {
    backend.drain_denials(m, 0).await.expect("drain").denials
}
