use agentkit_engine::*;
use agentkit_store::{SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

type AgentHost = agentkit_engine::AgentHost<agentkit_store::SqliteStore>;
type ExecutionContext = agentkit_engine::ExecutionContext<agentkit_store::SqliteStore>;
type EngineError = agentkit_engine::EngineError<agentkit_store::StoreError>;
type EngineResult<T> = Result<T, EngineError>;
type HostOptions = agentkit_engine::HostOptions<agentkit_store::SqliteStore>;

/// Models a run loop: each "tool body" starts only if the run is not already
/// cancelled, then waits for cancellation.
struct ToolRunner {
    started: mpsc::UnboundedSender<()>,
    tools_started: AtomicUsize,
}

#[async_trait]
impl TaskExecutor<agentkit_store::SqliteStore> for ToolRunner {
    async fn execute(&self, context: ExecutionContext) -> EngineResult<()> {
        context.check_cancelled()?;
        self.tools_started.fetch_add(1, Ordering::SeqCst);
        self.started.send(()).unwrap();
        // Stands in for a tool body blocked mid-call.
        context.cancellation.cancelled().await;
        // Only a user cancel reaches here; a hard stop drops this future
        // before it can write the terminal event below.
        // Best effort: a durable user cancel may already have fenced the lease.
        let _ = context
            .emit("run.cancelled", json!({"reason":"aborted"}))
            .await;
        Err(EngineError::Cancelled)
    }
}

fn task() -> SubmitTask {
    SubmitTask {
        task_id: "t".into(),
        kind: "test".into(),
        scope_id: "t".into(),
        payload: json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

fn host(store: SqliteStore, executor: Arc<ToolRunner>) -> AgentHost {
    // 90 ms: the forced-stop test grades what the worker does while its lease is
    // actively short-lived, so the sweep and the fencing window are in play.
    host_with_lease(store, executor, 90)
}

/// Same host at a chosen lease TTL. A test that does not grade fencing must not
/// inherit a TTL short enough for a loaded host to expire the worker's lease
/// mid-run — that turns correct fencing into a false verdict.
fn host_with_lease(store: SqliteStore, executor: Arc<ToolRunner>, lease_ttl_ms: i64) -> AgentHost {
    let options = HostOptions {
        lease_ttl_ms,
        poll_interval_ms: 10,
        ..HostOptions::default()
    };
    AgentHost::new(
        store,
        HashMap::from([(
            "test".into(),
            executor as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
        )]),
        Arc::new(SystemRuntime),
        options,
    )
    .unwrap()
}

fn executor() -> (Arc<ToolRunner>, mpsc::UnboundedReceiver<()>) {
    let (started, receiver) = mpsc::unbounded_channel();
    let runner = Arc::new(ToolRunner {
        started,
        tools_started: AtomicUsize::new(0),
    });
    (runner, receiver)
}

#[tokio::test]
async fn forced_shutdown_leaves_task_recoverable_not_cancelled() {
    let store = SqliteStore::memory(8).unwrap();
    let (runner, mut started) = executor();
    let first = host(store.clone(), runner.clone());
    first.submit(task()).await.unwrap();
    let mut worker = first.start_worker();
    started.recv().await.unwrap();

    worker.stop().await;
    let graceful = tokio::time::timeout(Duration::from_millis(100), worker.join()).await;
    assert!(graceful.is_err(), "run is still blocked past the deadline");

    worker.interrupt_active().await;
    tokio::time::timeout(Duration::from_secs(2), worker.join())
        .await
        .unwrap()
        .unwrap();

    let record = store.get_task("t").await.unwrap().unwrap();
    assert_eq!(record.status, TaskStatus::Running);
    assert!(!record.cancel_requested);
    assert_eq!(runner.tools_started.load(Ordering::SeqCst), 1);
    let events = store.events("t", None, 100).await.unwrap();
    assert!(
        events.is_empty(),
        "hard stop must write no terminal run event: {events:?}"
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    let (second_runner, _rx) = executor();
    let second = host(store.clone(), second_runner);
    let recovered = second.recover().await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].task_id, "t");
    let after = store.get_task("t").await.unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::Interrupted);
}

#[tokio::test]
async fn user_cancel_still_settles_cancelled() {
    let store = SqliteStore::memory(8).unwrap();
    let (runner, mut started) = executor();
    // A 5 s lease, not the file's default 90 ms: this test grades what a durable
    // user cancel settles, not what fencing looks like. At 90 ms a busy host
    // legitimately expires the worker's lease mid-run, and the honest `LeaseLost`
    // from `worker.shutdown()` becomes a false verdict — measured 2 failures in 3
    // runs, then 8 in 8 with the compile fix alone. Here the lease cannot lapse
    // inside the test, so any fence at all is a real regression and must fail.
    let host = host_with_lease(store.clone(), runner, 5_000);
    host.submit(task()).await.unwrap();
    let worker = host.start_worker();
    started.recv().await.unwrap();
    host.cancel("t").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), worker.shutdown())
        .await
        .expect("shutdown must settle within the deadline")
        .unwrap_or_else(|error| {
            panic!("a cancelled run must not be fenced at a 5 s lease: {error:?}")
        });
    let record = store.get_task("t").await.unwrap().unwrap();
    assert_eq!(record.status, TaskStatus::Cancelled);
}
