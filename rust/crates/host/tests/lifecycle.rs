use agentkit_host::*;
use agentkit_store::{SqliteStore, SubmitTask, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::Notify;

struct Completed;

#[async_trait]
impl TaskExecutor for Completed {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        context.emit("run.completed", json!({"iterations":1})).await
    }
}

struct Verify;

#[async_trait]
impl VerificationHook for Verify {
    async fn verify(&self, context: ExecutionContext) -> HostResult<VerificationReport> {
        assert_eq!(context.task.status, TaskStatus::Running);
        Ok(VerificationReport {
            status: "pass".into(),
            deficiencies: vec![],
        })
    }
}

fn task(id: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: "test".into(),
        scope_id: id.into(),
        payload: json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: None,
        exclusive_scope: false,
    }
}

fn host(executor: Arc<dyn TaskExecutor>, verify: bool) -> AgentHost {
    let options = HostOptions {
        verification: verify.then(|| Arc::new(Verify) as Arc<dyn VerificationHook>),
        ..HostOptions::default()
    };
    AgentHost::new(
        SqliteStore::memory(16).unwrap(),
        HashMap::from([("test".into(), executor)]),
        Arc::new(SystemRuntime),
        options,
    )
    .unwrap()
}

#[tokio::test]
async fn verification_is_durable_before_settlement() {
    let host = host(Arc::new(Completed), true);
    let run = host.submit(task("verify")).await.unwrap();
    assert!(host.run_once().await.unwrap());
    let mut events = run.subscribe(None);
    let mut kinds = vec![];
    while let Some(update) = events.recv().await {
        match update.unwrap() {
            RunUpdate::Event(event) => kinds.push(event.event["type"].as_str().unwrap().to_owned()),
            RunUpdate::Settled(task) => {
                assert_eq!(task.status, TaskStatus::Completed);
                assert_eq!(kinds, ["run.completed", "run.verification"]);
            }
        }
    }
}

#[tokio::test]
async fn dropped_subscription_does_not_cancel_work() {
    let host = host(Arc::new(Completed), false);
    let run = host.submit(task("detached")).await.unwrap();
    drop(run.subscribe(None));
    host.run_once().await.unwrap();
    assert_eq!(
        run.status().await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
}

struct ExternalEffect {
    committed: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl TaskExecutor for ExternalEffect {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        self.committed.notify_one();
        self.release.notified().await;
        context
            .emit(
                "run.tool.succeeded",
                json!({"toolCallId":"call","toolName":"write",
            "resultJson":"{}","sources":[],"warnings":[],"truncated":false}),
            )
            .await?;
        context.check_cancelled()
    }
}

#[tokio::test]
async fn cancellation_preserves_committed_effect_evidence() {
    let committed = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let host = host(
        Arc::new(ExternalEffect {
            committed: committed.clone(),
            release: release.clone(),
        }),
        false,
    );
    let run = host.submit(task("cancel")).await.unwrap();
    let worker = tokio::spawn({
        let host = host.clone();
        async move { host.run_once().await }
    });
    committed.notified().await;
    run.cancel().await.unwrap();
    release.notify_one();
    worker.await.unwrap().unwrap();
    let mut updates = run.subscribe(None);
    let first = updates.recv().await.unwrap().unwrap();
    assert!(
        matches!(first, RunUpdate::Event(event) if event.event["type"] == "run.tool.succeeded")
    );
    assert_eq!(
        run.status().await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
}
