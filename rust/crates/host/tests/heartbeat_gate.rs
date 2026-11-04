use agentkit_host::{
    AgentHost, ExecutionContext, HostOptions, HostResult, SystemRuntime, TaskExecutor,
};
use agentkit_store::{SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};

struct BlockedExecution {
    started: mpsc::UnboundedSender<()>,
    queued: mpsc::UnboundedSender<()>,
    go: Arc<Semaphore>,
}
#[async_trait]
impl TaskExecutor for BlockedExecution {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        self.started.send(()).unwrap();
        self.go.acquire().await.unwrap().forget();
        let progress = context.report_progress(json!({"done":true}));
        tokio::pin!(progress);
        let mut queued = Some(&self.queued);
        poll_fn(|cx| match progress.as_mut().poll(cx) {
            Poll::Pending => {
                if let Some(queued) = queued.take() {
                    queued.send(()).unwrap();
                }
                Poll::Pending
            }
            ready => ready,
        })
        .await
    }
}

#[tokio::test]
async fn heartbeat_does_not_pause_execution_waiting_for_store_gate() {
    let store = SqliteStore::memory(8).unwrap();
    let (started, mut started_rx) = mpsc::unbounded_channel();
    let (queued, mut queued_rx) = mpsc::unbounded_channel();
    let go = Arc::new(Semaphore::new(0));
    let executor: Arc<dyn TaskExecutor> = Arc::new(BlockedExecution {
        started,
        queued,
        go: go.clone(),
    });
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([("test".into(), executor)]),
        Arc::new(SystemRuntime),
        HostOptions {
            lease_ttl_ms: 3000,
            ..Default::default()
        },
    )
    .unwrap();
    host.submit(SubmitTask {
        task_id: "run".into(),
        kind: "test".into(),
        scope_id: "scope".into(),
        payload: json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    })
    .await
    .unwrap();
    let runner = tokio::spawn(async move { host.run_once().await });
    started_rx.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let tx = store.begin_transaction().await.unwrap();
    go.add_permits(1);
    queued_rx.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    tx.rollback().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), runner)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}
