//! The workspace service over the container adapter, the fake launcher
//! (which keeps the real launcher's one-container-per-generation rules)
//! and the real supervisor in-process: a first machine start that fails
//! after the launcher answered `ensured`, or a supervisor that dies while
//! the machine is idle, must not wedge the workspace.

mod support;

use std::sync::Arc;
use std::time::Duration;

use agentkit_egress::EgressPorts;
use agentkit_sandbox_container::ContainerBackend;
use agentkit_sandbox_proto::valid_workspace_id;
use agentkit_testkit::{InMemoryWorkspaceRepository, ManualClock};
use agentkit_workspaces::{
    AgentId, Binding, ExecRequest, Generation, MachineSpec, NetworkPolicy, OpContext, OperationId,
    OperationState, Pause, ResourceLimits, SandboxBackend, SandboxError, ServiceConfig, Settlement,
    Sha256, WorkspaceKey, WorkspaceKind, WorkspaceService, WorkspaceState,
};
use async_trait::async_trait;
use support::launcher::{FakeLauncher, Start, Starter};
use support::{DIGEST, backend_with, embedded};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Sleeps for real and moves the manual clock with it, so the service's
/// lease and deadline arithmetic sees time pass. The clock mints the
/// service's ids (`manual-N`): short ids a container can be named by.
struct RealPause(Arc<ManualClock>);

#[async_trait]
impl Pause for RealPause {
    async fn pause(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        self.0.advance(i64::try_from(ms).unwrap_or(i64::MAX));
    }
}

struct World {
    _dir: TempDir,
    launcher: FakeLauncher,
    /// The service's backend (a clone: same machines).
    backend: ContainerBackend,
    repo: Arc<InMemoryWorkspaceRepository>,
    service: WorkspaceService,
}

/// Generation 1's supervisor greets with another image digest, so the
/// adapter refuses its `hello` after the launcher answered `ensured`;
/// every later generation runs the real supervisor with the right digest.
fn first_start_fails(data: std::path::PathBuf) -> Starter {
    let bad = embedded::starter(data.clone(), "sha256:another-image");
    let good = embedded::starter(data, DIGEST);
    Arc::new(move |start: Start| {
        if start.generation == 1 {
            bad(start)
        } else {
            good(start)
        }
    })
}

fn world() -> World {
    world_with(first_start_fails)
}

/// A world whose launcher starts containers with `starter(<data dir>)`.
fn world_with(starter: impl FnOnce(std::path::PathBuf) -> Starter) -> World {
    let dir = tempfile::tempdir().unwrap();
    let starter = starter(dir.path().join("data"));
    let tune = |config: &mut agentkit_sandbox_container::ContainerBackendConfig| {
        config.stop_grace_ms = 500;
    };
    let (launcher, backend) = backend_with(dir.path(), starter, tune, EgressPorts::system());
    let clock = ManualClock::new(1_000_000);
    let repo = Arc::new(InMemoryWorkspaceRepository::new());
    let config = ServiceConfig {
        poll_interval_ms: 20,
        ..ServiceConfig::new(DIGEST)
    };
    let service = WorkspaceService::new(
        repo.clone(),
        Arc::new(backend.clone()),
        clock.clone(),
        config,
    )
    .unwrap()
    .with_pause(Arc::new(RealPause(clock)));
    World {
        _dir: dir,
        launcher,
        backend,
        repo,
        service,
    }
}

fn binding() -> Binding {
    Binding {
        key: WorkspaceKey::new("agent:a1"),
        kind: WorkspaceKind::AgentHome,
        owner_scope: "owner:1".into(),
        folder: None,
    }
}

fn ctx(op: &str) -> OpContext {
    OpContext {
        operation_id: OperationId::new(op),
        args_sha256: Sha256::of(op.as_bytes()),
        run_id: "run-a1".into(),
        agent_id: AgentId::new("a1"),
        cancellation: CancellationToken::new(),
    }
}

impl World {
    /// The launcher's `ensure` and `stop` calls, without the workspace id.
    fn lifecycle_calls(&self) -> Vec<String> {
        self.launcher
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("ensure ") || call.starts_with("stop "))
            .map(|call| {
                let mut words = call.split(' ');
                let kind = words.next().unwrap_or_default();
                let generation = words.nth(1).unwrap_or_default();
                format!("{kind} {generation}")
            })
            .collect()
    }

    async fn workspace(&self) -> agentkit_workspaces::Workspace {
        use agentkit_workspaces::WorkspaceRepository;
        self.repo
            .workspace_by_key(&binding().key)
            .await
            .unwrap()
            .unwrap()
    }

    async fn exec(&self, op: &str) -> Result<agentkit_workspaces::OperationOutcome, SandboxError> {
        let request = ExecRequest::new("printf ok > marker && cat marker");
        self.service.exec(&ctx(op), &binding(), request).await
    }
}

fn assert_succeeded(outcome: &agentkit_workspaces::OperationOutcome) {
    assert_eq!(
        outcome.settlement,
        Settlement::Settled(OperationState::Succeeded { exit: 0 }),
        "{outcome:?}"
    );
}

/// The adapter refuses generation 1's `hello` and stops its container
/// (proven), so the first exec answers `sandbox_unavailable`. The next
/// exec needs no owner action: the adapter answers generation 1 stale,
/// the service proves it stopped again, advances to generation 2 and the
/// command runs there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_first_start_moves_to_the_next_generation_on_the_next_call() {
    let w = world();
    let first = w.exec("op-1").await;
    assert!(
        matches!(first, Err(SandboxError::Unavailable { .. })),
        "{first:?}"
    );
    let ws = w.workspace().await;
    assert!(valid_workspace_id(ws.id.as_str()), "{}", ws.id);
    assert_eq!(ws.generation, Generation(1));
    assert_eq!(ws.state, WorkspaceState::Ready);
    assert_eq!(w.lifecycle_calls(), ["ensure 1", "stop 1"]);

    let second = w.exec("op-2").await.unwrap();
    assert_succeeded(&second);
    assert_eq!(second.generation, Generation(2));
    assert_eq!(
        w.lifecycle_calls(),
        ["ensure 1", "stop 1", "stop 1", "ensure 2"]
    );
    let ws = w.workspace().await;
    assert_eq!(ws.generation, Generation(2));
    assert_eq!(ws.state, WorkspaceState::Ready);
}

/// Same failure, but the adapter's own stop after the failed start comes
/// back unproven (the container is gone; the launcher could not prove
/// it). The launcher then answers `busy` for generation 1 (created once,
/// no container), which the adapter reports as generation 1 stale: the
/// service proves it stopped and the next call runs on generation 2
/// without an owner action, instead of answering `sandbox_unavailable`
/// for generation 1 for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_first_start_with_an_unproven_stop_does_not_wedge() {
    let w = world();
    w.launcher.unprove_next_stop();
    let first = w.exec("op-1").await;
    assert!(
        matches!(first, Err(SandboxError::Unavailable { .. })),
        "{first:?}"
    );
    assert_eq!(w.lifecycle_calls(), ["ensure 1", "stop 1"]);
    assert_eq!(w.workspace().await.state, WorkspaceState::Ready);

    let second = w.exec("op-2").await.unwrap();
    assert_succeeded(&second);
    assert_eq!(second.generation, Generation(2));
    assert_eq!(
        w.lifecycle_calls(),
        ["ensure 1", "stop 1", "ensure 1", "stop 1", "ensure 2"],
        "busy for generation 1 is answered stale, proven stopped and passed"
    );
    let ws = w.workspace().await;
    assert_eq!(ws.generation, Generation(2));
    assert_eq!(ws.state, WorkspaceState::Ready);
}

/// S1: the supervisor of an idle machine dies (no operation in flight, so
/// nothing is uncertain). The adapter answers that generation stale, not
/// unavailable, so the next exec needs no owner action and no eviction:
/// the service proves generation 1 stopped, advances, and the command runs
/// on generation 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_supervisor_that_dies_while_idle_does_not_wedge_the_workspace() {
    let w = world_with(|data| embedded::starter(data, DIGEST));
    let first = w.exec("op-1").await.unwrap();
    assert_succeeded(&first);
    assert_eq!(first.generation, Generation(1));
    let ws = w.workspace().await;
    w.launcher.kill(ws.id.as_str());

    // Wait until the adapter has seen the channel close: its answer for
    // generation 1 stops being the live handle.
    let spec = MachineSpec {
        workspace_id: ws.id.clone(),
        generation: Generation(1),
        image_digest: DIGEST.to_owned(),
        limits: ResourceLimits::DEFAULT,
        network: NetworkPolicy::default(),
    };
    let mut answer = w.backend.ensure_machine(&spec).await;
    for _ in 0..250 {
        if answer.is_err() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        answer = w.backend.ensure_machine(&spec).await;
    }
    assert_eq!(
        answer,
        Err(SandboxError::StaleGeneration {
            requested: Generation(1),
            current: Generation(1),
        })
    );

    let second = w.exec("op-2").await.unwrap();
    assert_succeeded(&second);
    assert_eq!(second.generation, Generation(2));
    assert_eq!(w.lifecycle_calls(), ["ensure 1", "stop 1", "ensure 2"]);
    let ws = w.workspace().await;
    assert_eq!(ws.generation, Generation(2));
    assert_eq!(ws.state, WorkspaceState::Ready);
}
