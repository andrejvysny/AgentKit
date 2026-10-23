use agentkit_host::*;
use agentkit_store::{SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
};

struct Clock(AtomicI64);
impl Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
    fn random(&self) -> f64 {
        0.0
    }
}

#[derive(Clone, Copy)]
enum FailureStyle {
    Safe,
    Unsafe,
    Rejected,
    RawNetwork,
    Aborted,
}

struct Script {
    calls: AtomicUsize,
    failures: usize,
    style: FailureStyle,
}

#[async_trait]
impl TaskExecutor for Script {
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.failures {
            return Ok(());
        }
        let failure = match self.style {
            FailureStyle::Safe => ExecutionFailure::safe_to_retry("temporary outage"),
            FailureStyle::Unsafe => ExecutionFailure {
                http_status: Some(503),
                ..ExecutionFailure::new("effect may have committed")
            },
            FailureStyle::Rejected => ExecutionFailure {
                http_status: Some(401),
                retry_safety: RetrySafety::SafeToRetry,
                ..ExecutionFailure::new("bad credentials")
            },
            FailureStyle::RawNetwork => {
                return Err(HostError::Execution(
                    "ECONNRESET after domain commit".into(),
                ));
            }
            FailureStyle::Aborted => return Err(HostError::Cancelled),
        };
        Err(failure.into())
    }
}

fn script(failures: usize, style: FailureStyle) -> Arc<Script> {
    Arc::new(Script {
        calls: AtomicUsize::new(0),
        failures,
        style,
    })
}

fn task(id: &str, scope: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: scope.into(),
        payload: json!({}),
        priority: 0,
        now: 1000,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

fn build_host(store: SqliteStore, executor: Arc<Script>, clock: Arc<Clock>) -> AgentHost {
    AgentHost::new(
        store,
        HashMap::from([("test".into(), executor as Arc<dyn TaskExecutor>)]),
        clock,
        HostOptions {
            lease_ttl_ms: 100,
            max_attempts: 3,
            retry_backoff: RetryBackoff {
                jitter_ratio: 0.0,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test]
async fn classified_retry_preserves_scope_order_backoff_and_attempt_history() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let executor = script(1, FailureStyle::Safe);
    let host = build_host(store.clone(), executor.clone(), clock.clone());
    host.submit(task("first", "scope")).await.unwrap();
    assert!(host.run_once().await.unwrap());
    let pending = store.get_task("first").await.unwrap().unwrap();
    assert_eq!(pending.status, TaskStatus::Running);
    assert!(pending.retry_pending);
    assert_eq!(pending.available_at, 2000);
    assert_eq!(pending.poison_count, 0);
    let mut sibling = task("second", "scope");
    sibling.priority = 999;
    host.submit(sibling).await.unwrap();
    clock.0.store(1999, Ordering::SeqCst);
    assert!(!host.run_once().await.unwrap());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    clock.0.store(2000, Ordering::SeqCst);
    assert!(host.run_once().await.unwrap());
    assert_eq!(
        store.get_task("first").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(
        store.get_task("second").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    let attempts = store.attempts("first").await.unwrap();
    assert_eq!(attempts[0].status, "failed");
    assert_eq!(attempts[1].status, "completed");
    assert_eq!(
        attempts[0].error.as_deref(),
        Some("retryable_flag: temporary outage")
    );
}

#[tokio::test]
async fn terminal_unknown_and_uncertain_effect_failures_never_replay() {
    for style in [
        FailureStyle::Unsafe,
        FailureStyle::Rejected,
        FailureStyle::RawNetwork,
    ] {
        let store = SqliteStore::memory(8).unwrap();
        let clock = Arc::new(Clock(AtomicI64::new(1000)));
        let executor = script(99, style);
        let host = build_host(store.clone(), executor.clone(), clock.clone());
        host.submit(task("run", "scope")).await.unwrap();
        host.run_once().await.unwrap();
        clock.0.store(9999, Ordering::SeqCst);
        assert!(!host.run_once().await.unwrap());
        let task = store.get_task("run").await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.attempt_count, 1);
        assert_eq!(task.poison_count, 0);
        assert!(task.dead_lettered_at.is_none());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn exhausted_transient_attempts_dead_letter_without_counting_crashes() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let executor = script(99, FailureStyle::Safe);
    let host = build_host(store.clone(), executor.clone(), clock.clone());
    host.submit(task("run", "scope")).await.unwrap();
    host.run_once().await.unwrap();
    clock.0.store(2000, Ordering::SeqCst);
    host.run_once().await.unwrap();
    clock.0.store(4000, Ordering::SeqCst);
    host.run_once().await.unwrap();
    let task = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.attempt_count, 3);
    assert_eq!(task.poison_count, 0);
    assert_eq!(
        task.dead_letter_reason.as_deref(),
        Some("retryable_flag: temporary outage")
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn cancellation_during_durable_backoff_never_starts_another_attempt() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let executor = script(99, FailureStyle::Safe);
    let host = build_host(store.clone(), executor.clone(), clock.clone());
    host.submit(task("run", "scope")).await.unwrap();
    host.run_once().await.unwrap();
    host.cancel("run").await.unwrap();
    clock.0.store(9999, Ordering::SeqCst);
    assert!(!host.run_once().await.unwrap());
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let store = SqliteStore::memory(8).unwrap();
    let host = build_host(store.clone(), script(99, FailureStyle::Aborted), clock);
    host.submit(task("abort", "scope")).await.unwrap();
    host.run_once().await.unwrap();
    assert_eq!(
        store.get_task("abort").await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
}

struct TestDatabase(PathBuf);
impl TestDatabase {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agentkit-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("store.db")
    }
}
impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn retry_crash_worker() {
    let Ok(path) = std::env::var("AGENTKIT_RETRY_CRASH_PATH") else {
        return;
    };
    let store = SqliteStore::open(path, 8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let host = build_host(store, script(99, FailureStyle::Safe), clock);
    host.submit(task("run", "scope")).await.unwrap();
    host.run_once().await.unwrap();
    std::fs::write(
        std::env::var("AGENTKIT_RETRY_READY_PATH").unwrap(),
        b"ready",
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn killed_runner_reopens_with_classified_retry_intent_and_original_deadline() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let database = TestDatabase::new();
    let ready = database.0.join("ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "retry_crash_worker", "--nocapture"])
        .env("AGENTKIT_RETRY_CRASH_PATH", database.path())
        .env("AGENTKIT_RETRY_READY_PATH", &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !ready.exists() {
        let _ = child.kill();
        panic!("retry crash worker did not become ready");
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let store = SqliteStore::open(database.path(), 8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1999)));
    let executor = script(0, FailureStyle::Safe);
    let automatic = AgentHost::new(
        store.clone(),
        HashMap::from([("test".into(), executor as Arc<dyn TaskExecutor>)]),
        clock.clone(),
        HostOptions {
            recovery: agentkit_store::RecoveryMode::Automatic,
            ..HostOptions::default()
        },
    )
    .unwrap();
    automatic.recover().await.unwrap();
    let pending = store.get_task("run").await.unwrap().unwrap();
    assert_eq!(pending.status, TaskStatus::Running);
    assert_eq!(pending.available_at, 2000);
    assert_eq!(pending.poison_count, 0);
    assert_eq!(store.attempts("run").await.unwrap()[0].status, "failed");
    assert!(!automatic.run_once().await.unwrap());
    clock.0.store(2000, Ordering::SeqCst);
    assert!(automatic.run_once().await.unwrap());
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}

#[tokio::test]
async fn manual_recovery_parks_retry_intent_until_explicit_resume() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let executor = script(1, FailureStyle::Safe);
    let host = build_host(store.clone(), executor.clone(), clock.clone());
    host.submit(task("run", "scope")).await.unwrap();
    host.run_once().await.unwrap();
    clock.0.store(1001, Ordering::SeqCst);
    host.recover().await.unwrap();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Interrupted
    );
    clock.0.store(9999, Ordering::SeqCst);
    assert!(!host.run_once().await.unwrap());
    host.resume("run").await.unwrap();
    assert!(host.run_once().await.unwrap());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn durable_backoff_keeps_its_concurrency_slot() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let executor = script(99, FailureStyle::Safe);
    let host = build_host(store.clone(), executor.clone(), clock);
    host.submit(task("first", "a")).await.unwrap();
    host.submit(task("second", "b")).await.unwrap();
    host.run_once().await.unwrap();
    host.run_once().await.unwrap();
    let mut queued = task("third", "c");
    queued.priority = 999;
    host.submit(queued).await.unwrap();
    assert!(!host.run_once().await.unwrap());
    assert_eq!(
        store
            .get_task("third")
            .await
            .unwrap()
            .unwrap()
            .attempt_count,
        0
    );
    host.cancel("first").await.unwrap();
    assert!(host.run_once().await.unwrap());
    assert_eq!(
        store
            .get_task("third")
            .await
            .unwrap()
            .unwrap()
            .attempt_count,
        1
    );
}

#[tokio::test]
async fn injected_runtime_randomness_determines_persisted_jitter_deadline() {
    let store = SqliteStore::memory(8).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let executor = script(1, FailureStyle::Safe);
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("test".into(), executor as Arc<dyn TaskExecutor>)]),
        clock.clone(),
        HostOptions::default(),
    )
    .unwrap();
    host.submit(task("run", "scope")).await.unwrap();
    host.run_once().await.unwrap();
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().available_at,
        1800
    );
    clock.0.store(1799, Ordering::SeqCst);
    assert!(!host.run_once().await.unwrap());
    clock.0.store(1800, Ordering::SeqCst);
    assert!(host.run_once().await.unwrap());
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}
