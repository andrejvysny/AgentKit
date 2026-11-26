//! Shared world of the workspace service tests: the in-memory repository
//! twin and the fake backend on one manual clock, a pause that lets
//! virtual time pass instead of sleeping, and a backend wrapper that
//! records machine starts and stop proofs so INV-5 can be checked.
#![allow(dead_code)]

pub mod hooks;
pub mod repo;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_kernel::Runtime;
use agentkit_testkit::{FakeSandboxBackend, InMemoryWorkspaceRepository, ManualClock, Program};
use agentkit_workspaces::{
    AgentId, BackendDescriptor, Binding, DenialBatch, DispatchAck, ExecRequest, FileChunk,
    Generation, ListEntries, Listing, MachineHandle, MachineSpec, NetworkPolicy, Observation,
    OpContext, OperationId, OperationOutcome, OperationRequest, OperationState, Pause, ReadFile,
    RelPath, SandboxBackend, SandboxError, ServiceConfig, Settlement, Sha256, StopProof,
    StorageUsage, WorkspaceId, WorkspaceKey, WorkspaceKind, WorkspaceRepository, WorkspaceService,
};
use async_trait::async_trait;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const IMAGE: &str = "sha256:image-1";
/// Exits 0 at once.
pub const TRUE: &str = "true";
/// Prints `hi` and exits 0.
pub const ECHO: &str = "echo hi";
/// Exits 3.
pub const EXIT_3: &str = "exit 3";
/// Runs until cancelled or its deadline.
pub const SLEEP: &str = "sleep infinity";
/// Exits 0 after 1 s.
pub const SLOW: &str = "sleep 1";
/// Exits 0 after 60 s.
pub const LONG: &str = "sleep 60";
/// Prints without end.
pub const YES: &str = "yes";
/// Connects to example.com:443.
pub const CURL: &str = "curl https://example.com/";
/// Connects to example.com:443 after 5 s and exits 0 after 6 s: the policy
/// in force at that moment decides, not the one at its start.
pub const CURL_LATER: &str = "sleep 5; curl https://example.com/";
/// Leaves a background process and exits 0 (F-7).
pub const BACKGROUND: &str = "(sleep 5; touch late) & exit 0";
/// Appends `x` to `mark.txt`: how often a command really ran.
pub const MARK: &str = "printf x >> mark.txt";

/// A pause that advances the shared manual clock. `freeze` parks every
/// pause until `thaw`: a worker stalls between polls, holding its lease.
pub struct ClockPause {
    clock: Arc<ManualClock>,
    frozen: AtomicBool,
    thaw: Notify,
}

impl ClockPause {
    pub fn new(clock: Arc<ManualClock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            frozen: AtomicBool::new(false),
            thaw: Notify::new(),
        })
    }

    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }

    pub fn thaw(&self) {
        self.frozen.store(false, Ordering::SeqCst);
        self.thaw.notify_waiters();
    }
}

#[async_trait]
impl Pause for ClockPause {
    async fn pause(&self, ms: u64) {
        loop {
            let thawed = self.thaw.notified();
            if !self.frozen.load(Ordering::SeqCst) {
                break;
            }
            thawed.await;
        }
        tokio::task::yield_now().await;
        self.clock.advance(i64::try_from(ms).unwrap_or(i64::MAX));
    }
}

#[derive(Default)]
struct Proofs {
    started: BTreeSet<(WorkspaceId, Generation)>,
    stopped: BTreeSet<(WorkspaceId, Generation)>,
    violations: Vec<String>,
}

/// The fake behind a recorder: every machine start and every `Stopped`
/// proof. Recorded as an INV-5 violation: starting a generation while an
/// older started one of the same workspace has no proof, and starting a
/// generation at or below one already proven stopped.
pub struct Recording {
    pub fake: FakeSandboxBackend,
    proofs: Mutex<Proofs>,
}

impl Recording {
    fn proofs(&self) -> std::sync::MutexGuard<'_, Proofs> {
        self.proofs.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Violations seen so far.
    pub fn violations(&self) -> Vec<String> {
        self.proofs().violations.clone()
    }

    /// True when a machine of `(ws, generation)` was ever started.
    pub fn started(&self, ws: &WorkspaceId, generation: Generation) -> bool {
        self.proofs().started.contains(&(ws.clone(), generation))
    }

    /// True when `(ws, generation)` was ever proven stopped.
    pub fn proven(&self, ws: &WorkspaceId, generation: Generation) -> bool {
        self.proofs().stopped.contains(&(ws.clone(), generation))
    }
}

#[async_trait]
impl SandboxBackend for Recording {
    fn describe(&self) -> BackendDescriptor {
        self.fake.describe()
    }

    async fn ensure_machine(&self, spec: &MachineSpec) -> Result<MachineHandle, SandboxError> {
        {
            let mut proofs = self.proofs();
            let unproven: Vec<Generation> = proofs
                .started
                .iter()
                .filter(|(ws, g)| ws == &spec.workspace_id && *g < spec.generation)
                .filter(|key| !proofs.stopped.contains(*key))
                .map(|(_, g)| *g)
                .collect();
            for g in unproven {
                let text = format!(
                    "{} generation {} asked to start while generation {g} is unproven",
                    spec.workspace_id, spec.generation
                );
                proofs.violations.push(text);
            }
        }
        let handle = self.fake.ensure_machine(spec).await?;
        let key = (spec.workspace_id.clone(), spec.generation);
        let mut proofs = self.proofs();
        let passed = proofs
            .stopped
            .iter()
            .find(|(ws, g)| ws == &spec.workspace_id && *g >= spec.generation)
            .map(|(_, g)| *g);
        if let Some(g) = passed {
            let text = format!(
                "{} generation {} started after generation {g} was proven stopped",
                spec.workspace_id, spec.generation
            );
            proofs.violations.push(text);
        }
        proofs.started.insert(key);
        Ok(handle)
    }

    async fn dispatch(
        &self,
        m: &MachineHandle,
        op: &OperationRequest,
    ) -> Result<DispatchAck, SandboxError> {
        self.fake.dispatch(m, op).await
    }

    async fn observe(
        &self,
        m: &MachineHandle,
        id: &OperationId,
        after_seq: u64,
    ) -> Result<Observation, SandboxError> {
        self.fake.observe(m, id, after_seq).await
    }

    async fn cancel(&self, m: &MachineHandle, id: &OperationId) -> Result<(), SandboxError> {
        self.fake.cancel(m, id).await
    }

    async fn read_file(
        &self,
        m: &MachineHandle,
        req: &ReadFile,
    ) -> Result<FileChunk, SandboxError> {
        self.fake.read_file(m, req).await
    }

    async fn list(&self, m: &MachineHandle, req: &ListEntries) -> Result<Listing, SandboxError> {
        self.fake.list(m, req).await
    }

    async fn stop_machine(
        &self,
        ws: &WorkspaceId,
        generation: Generation,
    ) -> Result<StopProof, SandboxError> {
        let proof = self.fake.stop_machine(ws, generation).await?;
        if proof == StopProof::Stopped {
            self.proofs().stopped.insert((ws.clone(), generation));
        }
        Ok(proof)
    }

    async fn apply_network_policy(
        &self,
        m: &MachineHandle,
        p: &NetworkPolicy,
    ) -> Result<(), SandboxError> {
        self.fake.apply_network_policy(m, p).await
    }

    async fn drain_denials(
        &self,
        m: &MachineHandle,
        cursor: u64,
    ) -> Result<DenialBatch, SandboxError> {
        self.fake.drain_denials(m, cursor).await
    }

    async fn storage_usage(&self, ws: &WorkspaceId) -> Result<StorageUsage, SandboxError> {
        self.fake.storage_usage(ws).await
    }

    async fn reset_home(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        self.fake.reset_home(ws).await
    }

    async fn destroy_storage(&self, ws: &WorkspaceId) -> Result<(), SandboxError> {
        self.fake.destroy_storage(ws).await
    }
}

/// The repository twin and the fake on one clock. A "restart" is a new
/// service over the same repository and backend.
pub struct World {
    pub clock: Arc<ManualClock>,
    pub repo: Arc<InMemoryWorkspaceRepository>,
    pub fake: FakeSandboxBackend,
    pub backend: Arc<Recording>,
    pub config: ServiceConfig,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl World {
    pub fn new() -> Self {
        let clock = ManualClock::new(1_000_000);
        let fake = FakeSandboxBackend::with_clock(clock.clone());
        script(&fake);
        let backend = Arc::new(Recording {
            fake: fake.clone(),
            proofs: Mutex::default(),
        });
        Self {
            clock,
            repo: Arc::new(InMemoryWorkspaceRepository::new()),
            fake,
            backend,
            config: ServiceConfig::new(IMAGE),
        }
    }

    /// A fresh service instance (a restart when one existed before).
    pub fn service(&self) -> (Arc<WorkspaceService>, Arc<ClockPause>) {
        let pause = ClockPause::new(self.clock.clone());
        let service = WorkspaceService::new(
            self.repo.clone(),
            self.backend.clone(),
            self.clock.clone(),
            self.config.clone(),
        )
        .expect("the test config is valid")
        .with_pause(pause.clone());
        (Arc::new(service), pause)
    }

    /// A fresh service over `backend` (usually a wrapper of
    /// [`World::backend`]) and the world's repository.
    pub fn service_with(&self, backend: Arc<dyn SandboxBackend>) -> Arc<WorkspaceService> {
        self.service_over(self.repo.clone(), backend, self.config.clone())
    }

    /// [`World::service_with`], and the pause it waits through.
    pub fn service_paused(
        &self,
        backend: Arc<dyn SandboxBackend>,
    ) -> (Arc<WorkspaceService>, Arc<ClockPause>) {
        let pause = ClockPause::new(self.clock.clone());
        let service = WorkspaceService::new(
            self.repo.clone(),
            backend,
            self.clock.clone(),
            self.config.clone(),
        )
        .expect("the test config is valid")
        .with_pause(pause.clone());
        (Arc::new(service), pause)
    }

    /// A fresh service of another host on the same store.
    pub fn service_on_host(&self, host: &str) -> Arc<WorkspaceService> {
        let config = ServiceConfig {
            host_id: host.to_owned(),
            ..self.config.clone()
        };
        self.service_over(self.repo.clone(), self.backend.clone(), config)
    }

    /// A fresh service over any repository and backend.
    pub fn service_over(
        &self,
        repo: Arc<dyn WorkspaceRepository>,
        backend: Arc<dyn SandboxBackend>,
        config: ServiceConfig,
    ) -> Arc<WorkspaceService> {
        let pause = ClockPause::new(self.clock.clone());
        let service = WorkspaceService::new(repo, backend, self.clock.clone(), config)
            .expect("the test config is valid")
            .with_pause(pause);
        Arc::new(service)
    }

    pub fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// INV-5 over the stored generations: every generation below the
    /// current one that ever had a machine was proven stopped.
    pub async fn generations_proven(&self) -> Vec<String> {
        let mut bad = self.backend.violations();
        for ws in self.repo.list_workspaces().await.expect("list") {
            for g in 1..ws.generation.0 {
                let g = Generation(g);
                if self.backend.started(&ws.id, g) && !self.backend.proven(&ws.id, g) {
                    bad.push(format!("{} advanced past {g} without a proof", ws.id));
                }
            }
        }
        bad
    }

    /// The stored record of `binding`.
    pub async fn workspace(&self, binding: &Binding) -> agentkit_workspaces::Workspace {
        self.repo
            .workspace_by_key(&binding.key)
            .await
            .expect("lookup")
            .expect("workspace exists")
    }
}

fn script(fake: &FakeSandboxBackend) {
    fake.script(TRUE, Program::exit(0))
        .script(ECHO, Program::exit(0).stdout("hi\n"))
        .script(EXIT_3, Program::exit(3))
        .script(SLEEP, Program::hang())
        .script(SLOW, Program::exit(0).runs_for(1_000))
        .script(LONG, Program::exit(0).runs_for(60_000))
        .script(YES, Program::endless("y\n"))
        .script(CURL, Program::connect("example.com", 443))
        .script(
            CURL_LATER,
            Program::connect("example.com", 443)
                .connects_after(5_000)
                .runs_for(6_000),
        )
        .script(BACKGROUND, Program::exit(0).leave_background())
        .script(MARK, Program::exit(0).append("mark.txt", "x"));
}

/// An agent home binding.
pub fn home(agent: &str) -> Binding {
    Binding {
        key: WorkspaceKey::new(format!("agent:{agent}")),
        kind: WorkspaceKind::AgentHome,
        owner_scope: "owner:1".into(),
        folder: None,
    }
}

/// A project chat binding: one workspace per project, a folder per chat.
pub fn chat(project: &str, chat: &str) -> Binding {
    Binding {
        key: WorkspaceKey::new(format!("chats:project:{project}")),
        kind: WorkspaceKind::ProjectChats,
        owner_scope: "owner:1".into(),
        folder: Some(rel(&format!("chats/{chat}"))),
    }
}

pub fn rel(text: &str) -> RelPath {
    RelPath::parse(text).expect("valid relative path")
}

/// A holder of an earlier boot of this host: recovery knows it is gone.
pub fn dead_holder(agent: &str) -> agentkit_workspaces::LeaseHolder {
    agentkit_workspaces::LeaseHolder {
        agent_id: AgentId::new(agent),
        run_id: format!("run-dead-{agent}"),
        label: agent.to_owned(),
        host_id: "local".into(),
        boot_id: "boot-before-the-restart".into(),
    }
}

/// The context of operation `op` by `agent` in run `run-<agent>`.
pub fn ctx(op: &str, agent: &str) -> OpContext {
    OpContext {
        operation_id: OperationId::new(op),
        args_sha256: Sha256::of(op.as_bytes()),
        run_id: format!("run-{agent}"),
        agent_id: AgentId::new(agent),
        cancellation: CancellationToken::new(),
    }
}

pub fn exec(command: &str) -> ExecRequest {
    ExecRequest::new(command)
}

pub fn settled(outcome: &OperationOutcome) -> OperationState {
    match &outcome.settlement {
        Settlement::Settled(state) => *state,
        other => panic!("expected a receipt, got {other:?}"),
    }
}

pub fn assert_uncertain(outcome: &OperationOutcome) {
    assert!(
        outcome.settlement.is_uncertain(),
        "expected uncertain, got {:?}",
        outcome.settlement
    );
}

pub fn assert_fenced<T: std::fmt::Debug>(outcome: Result<T, SandboxError>) {
    match outcome {
        Err(error @ SandboxError::Fenced { .. }) => assert_eq!(error.code(), "sandbox_fenced"),
        other => panic!("expected sandbox_fenced, got {other:?}"),
    }
}

pub const SUCCEEDED: OperationState = OperationState::Succeeded { exit: 0 };
