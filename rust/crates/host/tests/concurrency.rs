use agentkit_host::*;
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
use tokio::sync::{Semaphore, mpsc};

struct Blocking {
    started: mpsc::UnboundedSender<String>,
    permits: Arc<Semaphore>,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

#[async_trait]
impl TaskExecutor for Blocking {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        self.started.send(context.task.task_id.clone()).unwrap();
        self.permits.acquire().await.unwrap().forget();
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }
}

fn task(id: &str, scope: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: scope.into(),
        payload: json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

fn setup() -> (AgentHost, Arc<Blocking>, mpsc::UnboundedReceiver<String>) {
    let (started, receiver) = mpsc::unbounded_channel();
    let executor = Arc::new(Blocking {
        started,
        permits: Arc::new(Semaphore::new(0)),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
    });
    let host = AgentHost::new(
        SqliteStore::memory(8).unwrap(),
        HashMap::from([("test".into(), executor.clone() as Arc<dyn TaskExecutor>)]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    (host, executor, receiver)
}

async fn next(receiver: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn independent_scopes_overlap_and_stop_drains_without_new_admissions() {
    let (host, executor, mut started) = setup();
    for id in ["a", "b", "c"] {
        host.submit(task(id, id)).await.unwrap();
    }
    let worker = host.start_worker();
    let mut running = vec![next(&mut started).await, next(&mut started).await];
    running.sort();
    assert_eq!(running, vec!["a", "b"]);
    assert_eq!(executor.active.load(Ordering::SeqCst), 2);
    assert_eq!(executor.maximum.load(Ordering::SeqCst), 2);
    worker.stop().await;
    executor.permits.add_permits(2);
    tokio::time::timeout(Duration::from_secs(2), worker.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        host.store().get_task("c").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    assert_eq!(executor.active.load(Ordering::SeqCst), 0);
    assert!(started.try_recv().is_err());
}

#[tokio::test]
async fn same_scope_tasks_remain_serial_under_two_worker_default() {
    let (host, executor, mut started) = setup();
    host.submit(task("a", "shared")).await.unwrap();
    host.submit(task("b", "shared")).await.unwrap();
    let worker = host.start_worker();
    assert_eq!(next(&mut started).await, "a");
    assert_eq!(
        host.store().get_task("b").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
    executor.permits.add_permits(1);
    assert_eq!(next(&mut started).await, "b");
    worker.stop().await;
    executor.permits.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), worker.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(executor.maximum.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_run_once_calls_share_the_same_durable_concurrency_limit() {
    let (host, executor, mut started) = setup();
    for id in ["a", "b", "c"] {
        host.submit(task(id, id)).await.unwrap();
    }
    let mut calls = tokio::task::JoinSet::new();
    for _ in 0..3 {
        let host = host.clone();
        calls.spawn(async move { host.run_once().await });
    }
    next(&mut started).await;
    next(&mut started).await;
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), calls.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap()
    );
    executor.permits.add_permits(2);
    while let Some(result) = calls.join_next().await {
        assert!(result.unwrap().unwrap());
    }
    assert_eq!(executor.maximum.load(Ordering::SeqCst), 2);
    assert_eq!(
        host.store().get_task("c").await.unwrap().unwrap().status,
        TaskStatus::Queued
    );
}
