use agentkit_host::*;
use agentkit_store::{SqliteStore, StoreError, SubmitTask, TaskRecord, TaskStatus};
use async_trait::async_trait;
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore, mpsc};

#[path = "generic_tasks/lineage.rs"]
mod lineage;

fn task(id: &str, kind: &str) -> SubmitTask {
    SubmitTask {
        task_id: id.into(),
        kind: kind.into(),
        scope_id: id.into(),
        payload: json!({}),
        priority: 0,
        now: 0,
        available_at: None,
        depends_on: vec![],
        parent_task_id: Some("caller-cannot-forge-parent".into()),
        exclusive_scope: false,
    }
}

struct Fanout {
    captured: mpsc::UnboundedSender<ExecutionContext>,
    gate: Option<Arc<Semaphore>>,
}

#[async_trait]
impl TaskExecutor for Fanout {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        self.captured.send(context.clone()).unwrap();
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
        context.report_progress(json!({"phase":"spawning"})).await?;
        context.emit("demo.parent.started", json!({})).await?;
        let mut children = Vec::new();
        for index in 0..2 {
            let mut child = task(&format!("parent-leaf-{index}"), "demo.leaf");
            child.payload =
                json!({"explode":index==1 && context.task.payload["failLeaf"]==true,"value":index});
            children.push(context.spawn_child(child).await?.task_id);
        }
        let mut join = task("parent-join", "demo.join");
        join.depends_on = children;
        join.priority = i32::MAX;
        context.spawn_child(join).await?;
        context
            .report_progress(json!({"phase":"submitted","children":3}))
            .await?;
        context
            .emit("demo.parent.completed", json!({"children":3}))
            .await
    }
}

struct Leaves {
    started: mpsc::UnboundedSender<String>,
    gate: Arc<Semaphore>,
    trace: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl TaskExecutor for Leaves {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        self.trace.lock().await.push(context.task.task_id.clone());
        context.report_progress(json!({"done":0,"total":1})).await?;
        context
            .emit("demo.leaf.started", context.task.payload.clone())
            .await?;
        self.started.send(context.task.task_id.clone()).unwrap();
        tokio::select! {
            permit=self.gate.acquire()=>permit.unwrap().forget(),
            _=context.cancellation.cancelled()=>return Err(HostError::Cancelled),
        }
        if context.task.payload["explode"] == true {
            return Err(HostError::Execution("leaf work exploded".into()));
        }
        context.report_progress(json!({"done":1,"total":1})).await?;
        context
            .emit(
                "demo.leaf.completed",
                json!({"value":context.task.payload["value"]}),
            )
            .await
    }
}

struct Join {
    store: SqliteStore,
    trace: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl TaskExecutor for Join {
    async fn execute(&self, context: ExecutionContext) -> HostResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for dependency in &context.task.depends_on {
            assert_eq!(
                self.store.get_task(dependency).await?.unwrap().status,
                TaskStatus::Completed
            );
        }
        self.trace.lock().await.push(context.task.task_id.clone());
        context.report_progress(json!({"joined":2})).await?;
        context
            .emit("demo.join.completed", json!({"joined":2}))
            .await
    }
}

struct Environment {
    host: AgentHost,
    parent: mpsc::UnboundedReceiver<ExecutionContext>,
    leaves: mpsc::UnboundedReceiver<String>,
    leaf_gate: Arc<Semaphore>,
    trace: Arc<Mutex<Vec<String>>>,
    join_calls: Arc<AtomicUsize>,
}

fn environment(store: SqliteStore, parent_gate: Option<Arc<Semaphore>>) -> Environment {
    let (captured, parent) = mpsc::unbounded_channel();
    let (started, leaves) = mpsc::unbounded_channel();
    let leaf_gate = Arc::new(Semaphore::new(0));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let join_calls = Arc::new(AtomicUsize::new(0));
    let executors: HashMap<String, Arc<dyn TaskExecutor>> = HashMap::from([
        (
            "demo.fanout".into(),
            Arc::new(Fanout {
                captured,
                gate: parent_gate,
            }) as Arc<dyn TaskExecutor>,
        ),
        (
            "demo.leaf".into(),
            Arc::new(Leaves {
                started,
                gate: leaf_gate.clone(),
                trace: trace.clone(),
            }) as Arc<dyn TaskExecutor>,
        ),
        (
            "demo.join".into(),
            Arc::new(Join {
                store: store.clone(),
                trace: trace.clone(),
                calls: join_calls.clone(),
            }) as Arc<dyn TaskExecutor>,
        ),
    ]);
    let host = AgentHost::new(
        store,
        executors,
        Arc::new(SystemRuntime),
        HostOptions {
            poll_interval_ms: 5,
            ..Default::default()
        },
    )
    .unwrap();
    Environment {
        host,
        parent,
        leaves,
        leaf_gate,
        trace,
        join_calls,
    }
}

async fn terminal(store: &SqliteStore, id: &str) -> TaskRecord {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(task) = store.get_task(id).await.unwrap()
                && task.status.is_terminal()
            {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

async fn next<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

fn parent_task(fail_leaf: bool) -> SubmitTask {
    let mut input = task("parent", "demo.fanout");
    input.parent_task_id = None;
    input.payload = json!({"failLeaf":fail_leaf});
    input
}

struct Database(PathBuf);
impl Database {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("agentkit-generic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn store(&self) -> SqliteStore {
        SqliteStore::open(self.0.join("store.db"), 16).unwrap()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn assert_stream(store: &SqliteStore, id: &str, expected: &[&str]) {
    let events = store.events(id, None, 100).await.unwrap();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        expected
    );
    for (sequence, event) in events.iter().enumerate() {
        assert_eq!(event.seq, sequence as f64);
        agentkit_contracts::validate_schema("TaskEventEnvelopeSchema", &event.event).unwrap();
        assert!(event.event["attemptId"].is_string());
    }
}

#[tokio::test]
async fn native_worker_fans_out_children_reports_progress_and_joins_only_complete_results() {
    let database = Database::new();
    let mut environment = environment(database.store(), None);
    environment.host.submit(parent_task(false)).await.unwrap();
    let worker = environment.host.start_worker();
    next(&mut environment.parent).await;
    let mut leaves = vec![
        next(&mut environment.leaves).await,
        next(&mut environment.leaves).await,
    ];
    leaves.sort();
    assert_eq!(leaves, vec!["parent-leaf-0", "parent-leaf-1"]);
    let parent = terminal(environment.host.store(), "parent").await;
    assert_eq!(parent.status, TaskStatus::Completed);
    assert_eq!(
        parent.progress,
        Some(json!({"phase":"submitted","children":3}))
    );
    let join = environment
        .host
        .store()
        .get_task("parent-join")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(join.status, TaskStatus::Queued);
    assert_eq!(join.attempt_count, 0);
    assert_eq!(environment.join_calls.load(Ordering::SeqCst), 0);
    environment.leaf_gate.add_permits(2);
    let join = terminal(environment.host.store(), "parent-join").await;
    worker.shutdown().await.unwrap();
    assert_eq!(join.status, TaskStatus::Completed);
    assert_eq!(join.progress, Some(json!({"joined":2})));
    assert_eq!(
        environment.trace.lock().await.last().map(String::as_str),
        Some("parent-join")
    );
    let children: Vec<_> = environment
        .host
        .store()
        .list_tasks(None)
        .await
        .unwrap()
        .into_iter()
        .filter(|task| task.parent_task_id.as_deref() == Some("parent"))
        .collect();
    assert_eq!(children.len(), 3);
    assert!(
        children
            .iter()
            .all(|task| task.status == TaskStatus::Completed)
    );
    assert_eq!(join.depends_on, vec!["parent-leaf-0", "parent-leaf-1"]);
    assert_stream(
        environment.host.store(),
        "parent",
        &["demo.parent.started", "demo.parent.completed"],
    )
    .await;
    assert_stream(
        environment.host.store(),
        "parent-join",
        &["demo.join.completed"],
    )
    .await;
    for id in leaves {
        assert_eq!(
            environment
                .host
                .store()
                .get_task(&id)
                .await
                .unwrap()
                .unwrap()
                .progress,
            Some(json!({"done":1,"total":1}))
        );
        assert_stream(
            environment.host.store(),
            &id,
            &["demo.leaf.started", "demo.leaf.completed"],
        )
        .await;
    }
}

#[tokio::test]
async fn native_worker_fails_dependency_join_without_dispatching_it_after_leaf_failure() {
    let mut environment = environment(SqliteStore::memory(16).unwrap(), None);
    environment.host.submit(parent_task(true)).await.unwrap();
    let worker = environment.host.start_worker();
    next(&mut environment.parent).await;
    next(&mut environment.leaves).await;
    next(&mut environment.leaves).await;
    environment.leaf_gate.add_permits(2);
    let join = terminal(environment.host.store(), "parent-join").await;
    let failed = terminal(environment.host.store(), "parent-leaf-1").await;
    terminal(environment.host.store(), "parent-leaf-0").await;
    worker.shutdown().await.unwrap();
    assert_eq!(join.status, TaskStatus::Failed);
    assert_eq!(
        join.error.as_deref(),
        Some("dependency_failed: parent-leaf-1")
    );
    assert_eq!(join.attempt_count, 0);
    assert_eq!(environment.join_calls.load(Ordering::SeqCst), 0);
    assert!(
        environment
            .host
            .store()
            .events("parent-join", None, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(failed.attempt_count, 1);
    assert!(failed.dead_lettered_at.is_none());
}

#[tokio::test]
async fn completed_parent_context_cannot_spawn_or_rewrite_progress_with_expired_ownership() {
    let mut environment = environment(SqliteStore::memory(16).unwrap(), None);
    environment.host.submit(parent_task(false)).await.unwrap();
    let worker = environment.host.start_worker();
    let parent = next(&mut environment.parent).await;
    terminal(environment.host.store(), "parent").await;
    assert!(matches!(
        parent.spawn_child(task("late", "demo.leaf")).await,
        Err(HostError::Store(StoreError::LeaseLost(_)))
    ));
    assert!(matches!(
        parent.report_progress(json!({"corrupt":true})).await,
        Err(HostError::Store(StoreError::LeaseLost(_)))
    ));
    assert!(
        environment
            .host
            .store()
            .get_task("late")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        environment
            .host
            .store()
            .get_task("parent")
            .await
            .unwrap()
            .unwrap()
            .progress,
        Some(json!({"phase":"submitted","children":3}))
    );
    environment.leaf_gate.add_permits(2);
    terminal(environment.host.store(), "parent-join").await;
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_active_parent_cannot_spawn_children_or_report_new_progress() {
    let gate = Arc::new(Semaphore::new(0));
    let mut environment = environment(SqliteStore::memory(16).unwrap(), Some(gate.clone()));
    environment.host.submit(parent_task(false)).await.unwrap();
    let worker = environment.host.start_worker();
    let parent = next(&mut environment.parent).await;
    environment.host.cancel("parent").await.unwrap();
    assert!(matches!(
        parent.spawn_child(task("after-cancel", "demo.leaf")).await,
        Err(HostError::Cancelled)
    ));
    assert!(matches!(
        parent.report_progress(json!({"afterCancel":true})).await,
        Err(HostError::Cancelled)
    ));
    gate.add_permits(1);
    let task = terminal(environment.host.store(), "parent").await;
    worker.shutdown().await.unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(
        environment
            .host
            .store()
            .list_tasks(None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        environment
            .host
            .store()
            .events("parent", None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn queued_generic_task_cancellation_prevents_any_execution() {
    let mut environment = environment(SqliteStore::memory(16).unwrap(), None);
    let mut input = parent_task(false);
    input.available_at = Some(9_007_199_254_740_991);
    environment.host.submit(input).await.unwrap();
    environment.host.cancel("parent").await.unwrap();
    let worker = environment.host.start_worker();
    assert!(!environment.host.run_once().await.unwrap());
    worker.shutdown().await.unwrap();
    assert_eq!(
        terminal(environment.host.store(), "parent").await.status,
        TaskStatus::Cancelled
    );
    assert!(environment.parent.try_recv().is_err());
    assert!(
        environment
            .host
            .store()
            .events("parent", None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}
