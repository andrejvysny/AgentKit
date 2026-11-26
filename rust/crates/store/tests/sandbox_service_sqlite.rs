//! Smoke run of the workspace service (WP-1.5) over the SQLite repository
//! and the fake backend: a receipt, a lost ack made uncertain with the
//! generation advanced at the next use (the `mark_ready` rule in SQL), and
//! startup recovery. The full failure matrix runs on the in-memory twin in
//! `agentkit-workspaces`.

use std::sync::Arc;

use agentkit_kernel::Runtime;
use agentkit_store::{SqliteStore, SqliteWorkspaceRepository};
use agentkit_testkit::{FakeSandboxBackend, ManualClock, Program};
use agentkit_workspaces::{
    AgentId, Binding, ExecRequest, Generation, OpContext, OperationId, OperationPhase,
    OperationState, Pause, ServiceConfig, Settlement, Sha256, WorkspaceKey, WorkspaceKind,
    WorkspaceRepository, WorkspaceService, WorkspaceState,
};
use async_trait::async_trait;

/// Lets virtual time pass on the shared clock.
struct ClockPause(Arc<ManualClock>);

#[async_trait]
impl Pause for ClockPause {
    async fn pause(&self, ms: u64) {
        tokio::task::yield_now().await;
        self.0.advance(i64::try_from(ms).unwrap_or(i64::MAX));
    }
}

fn service(
    repo: &Arc<SqliteWorkspaceRepository>,
    fake: &FakeSandboxBackend,
    clock: &Arc<ManualClock>,
) -> WorkspaceService {
    WorkspaceService::new(
        repo.clone(),
        Arc::new(fake.clone()),
        clock.clone(),
        ServiceConfig::new("sha256:image-1"),
    )
    .expect("the default config is valid")
    .with_pause(Arc::new(ClockPause(clock.clone())))
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
    OpContext::new(
        OperationId::new(op),
        Sha256::of(op.as_bytes()),
        "run-a1",
        AgentId::new("a1"),
    )
}

#[tokio::test]
async fn the_service_runs_on_the_sqlite_repository() {
    let clock = ManualClock::new(1_000_000);
    let fake = FakeSandboxBackend::with_clock(clock.clone());
    fake.script("true", Program::exit(0));
    let store = SqliteStore::memory(16).expect("store");
    let repo = Arc::new(SqliteWorkspaceRepository::new(store));
    let svc = service(&repo, &fake, &clock);
    let done = svc
        .exec(&ctx("op-1"), &binding(), ExecRequest::new("true"))
        .await
        .unwrap();
    assert_eq!(
        done.settlement,
        Settlement::Settled(OperationState::Succeeded { exit: 0 })
    );
    fake.lose_next_ack();
    let lost = svc
        .exec(&ctx("op-2"), &binding(), ExecRequest::new("true"))
        .await
        .unwrap();
    assert!(lost.settlement.is_uncertain(), "{:?}", lost.settlement);
    let ws = repo
        .workspace_by_key(&binding().key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (ws.state, ws.generation),
        (WorkspaceState::Busy, Generation(1))
    );
    let row = repo.operation(&OperationId::new("op-2")).await.unwrap();
    assert_eq!(row.map(|row| row.phase), Some(OperationPhase::Uncertain));
    let restarted = service(&repo, &fake, &clock);
    let report = restarted.recover_all().await.unwrap();
    assert!(report.uncertain.is_empty() && report.leases_broken == 0);
    let next = restarted
        .exec(&ctx("op-3"), &binding(), ExecRequest::new("true"))
        .await
        .unwrap();
    assert_eq!(next.generation, Generation(2));
    assert_eq!(fake.dispatch_calls(&OperationId::new("op-2")), 1);
    assert!(clock.now_ms() >= 1_000_000);
}
