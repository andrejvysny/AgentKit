use agentkit_engine::*;
use agentkit_store::{ClaimInput, SqliteStore, StoreError, SubmitTask, TaskStatus};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

type AgentHost = agentkit_engine::AgentHost<agentkit_store::SqliteStore>;
type ExecutionContext = agentkit_engine::ExecutionContext<agentkit_store::SqliteStore>;
type EngineError = agentkit_engine::EngineError<agentkit_store::StoreError>;
type EngineResult<T> = Result<T, EngineError>;
type HostOptions = agentkit_engine::HostOptions<agentkit_store::SqliteStore>;

struct Clock(AtomicI64);

impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        "survivor".into()
    }
}

struct Idle;

#[async_trait]
impl TaskExecutor<agentkit_store::SqliteStore> for Idle {
    async fn execute(&self, _context: ExecutionContext) -> EngineResult<()> {
        Ok(())
    }
}

fn task(id: &str, exclusive: bool) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: "scope".into(),
        payload: serde_json::json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: exclusive,
    }
}

fn host(options: HostOptions) -> (AgentHost, Arc<Clock>) {
    let clock = Arc::new(Clock(AtomicI64::new(0)));
    let host = AgentHost::new(
        SqliteStore::memory(8).unwrap(),
        HashMap::from([(
            "test".into(),
            Arc::new(Idle) as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
        )]),
        clock.clone(),
        options,
    )
    .unwrap();
    (host, clock)
}

/// A task claimed by a "dead" foreign owner with a 1000 ms lease from t=0.
async fn orphan(host: &AgentHost, id: &str) {
    host.store().submit(task(id, true)).await.unwrap();
    let claimed = host
        .store()
        .claim(ClaimInput {
            owner_id: "dead".into(),
            now: 0,
            ttl_ms: 1000,
            kinds: vec!["test".into()],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .unwrap();
    assert!(claimed.is_some());
}

async fn status(host: &AgentHost, id: &str) -> TaskStatus {
    host.store().get_task(id).await.unwrap().unwrap().status
}

#[tokio::test]
async fn recover_expired_frees_orphan_after_lease_expiry() {
    let (host, clock) = host(HostOptions::default());
    orphan(&host, "a").await;
    clock.0.store(500, Ordering::SeqCst);
    assert!(host.recover_expired().await.unwrap().is_empty());
    assert_eq!(status(&host, "a").await, TaskStatus::Running);

    clock.0.store(1000, Ordering::SeqCst);
    let recovered = host.recover_expired().await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_ne!(status(&host, "a").await, TaskStatus::Running);

    // Manual recovery parks the task as interrupted; cancelling it frees the scope.
    assert!(matches!(
        host.store().submit(task("b", true)).await,
        Err(StoreError::ScopeBusy(_))
    ));
    host.cancel("a").await.unwrap();
    host.store().submit(task("b", true)).await.unwrap();
}

#[tokio::test]
async fn recover_expired_does_not_interrupt_queued() {
    let (host, clock) = host(HostOptions::default());
    host.store().submit(task("q", false)).await.unwrap();
    clock.0.store(100_000, Ordering::SeqCst);
    host.recover_expired().await.unwrap();
    assert_eq!(status(&host, "q").await, TaskStatus::Queued);
    // Startup recovery still interrupts it.
    host.recover().await.unwrap();
    assert_eq!(status(&host, "q").await, TaskStatus::Interrupted);
}

#[tokio::test]
async fn cancel_settles_expired_orphan_immediately() {
    let (host, clock) = host(HostOptions::default());
    orphan(&host, "a").await;
    clock.0.store(2000, Ordering::SeqCst);
    host.cancel("a").await.unwrap();
    assert_eq!(status(&host, "a").await, TaskStatus::Cancelled);
    host.store().submit(task("b", true)).await.unwrap();
}

#[tokio::test]
async fn cancel_keeps_unexpired_orphan_running_until_expiry() {
    let (host, clock) = host(HostOptions::default());
    orphan(&host, "a").await;
    clock.0.store(500, Ordering::SeqCst);
    host.cancel("a").await.unwrap();
    assert_eq!(status(&host, "a").await, TaskStatus::Running);
    clock.0.store(1000, Ordering::SeqCst);
    host.recover_expired().await.unwrap();
    assert_eq!(status(&host, "a").await, TaskStatus::Cancelled);
}

#[tokio::test]
async fn sweeper_recovers_expired_orphan_while_worker_runs() {
    let (host, clock) = host(HostOptions {
        recovery_sweep_interval_ms: 10,
        poll_interval_ms: 10,
        ..HostOptions::default()
    });
    orphan(&host, "a").await;
    let mut worker = host.start_worker();
    clock.0.store(5000, Ordering::SeqCst);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while status(&host, "a").await == TaskStatus::Running {
        assert!(tokio::time::Instant::now() < deadline, "sweeper never ran");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    worker.stop().await;
    worker.join().await.unwrap();
}
