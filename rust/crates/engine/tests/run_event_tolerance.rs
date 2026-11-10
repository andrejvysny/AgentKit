use agentkit_engine::{RunUpdate, SystemRuntime, TaskExecutor};
use agentkit_store::{SqliteStore, SubmitTask};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

type AgentHost = agentkit_engine::AgentHost<agentkit_store::SqliteStore>;
type ExecutionContext = agentkit_engine::ExecutionContext<agentkit_store::SqliteStore>;
type EngineError = agentkit_engine::EngineError<agentkit_store::StoreError>;
type EngineResult<T> = Result<T, EngineError>;
type HostOptions = agentkit_engine::HostOptions<agentkit_store::SqliteStore>;

struct Idle;
#[async_trait]
impl TaskExecutor<agentkit_store::SqliteStore> for Idle {
    async fn execute(&self, _context: ExecutionContext) -> EngineResult<()> {
        Ok(())
    }
}

fn event(seq: u64, kind: &str, data: Value) -> Value {
    json!({"seq":seq,"eventId":format!("event-{seq}"),"type":kind,"runId":"progress-log",
        "timestamp":"2026-11-06T12:00:00.000Z","contractVersion":"future","data":data})
}

/// Progress events and types this build does not know replay like any other
/// durable event: stored, paged and delivered in order, never a failure.
#[tokio::test]
async fn replay_delivers_progress_and_unknown_run_events_in_order() {
    let store = SqliteStore::memory(16).unwrap();
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "custom".into(),
            Arc::new(Idle) as Arc<dyn TaskExecutor<agentkit_store::SqliteStore>>,
        )]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let run = host
        .submit(SubmitTask {
            task_id: "progress-log".into(),
            kind: "custom".into(),
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
    host.run_once().await.unwrap();
    let lease = store
        .acquire_lease("progress-log", "late-events", "test", 1, 1000)
        .await
        .unwrap();
    let events = vec![
        event(
            0,
            "run.tool.preparing",
            json!({"toolCallId":"c1","toolName":"write","argsBytes":0,"step":1}),
        ),
        event(1, "run.progress", json!({"phase":"tool_args","step":1})),
        event(2, "run.future.event", json!({"anything":[1, 2]})),
        event(
            3,
            "run.tool.requested",
            json!({"toolCallId":"c1","toolName":"write",
            "argumentsJson":"{}"}),
        ),
    ];
    store
        .append_events(lease.fence(), events.clone(), 1)
        .await
        .unwrap();
    let mut updates = run.subscribe_after_number(None);
    let mut replayed = vec![];
    let mut settled = false;
    while let Some(update) = updates.recv().await {
        match update.unwrap() {
            RunUpdate::Event(record) => replayed.push(record.event),
            RunUpdate::Settled(_) => settled = true,
        }
    }
    assert!(settled);
    assert_eq!(
        replayed
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "run.tool.preparing",
            "run.progress",
            "run.future.event",
            "run.tool.requested"
        ]
    );
    assert_eq!(replayed[2]["data"], events[2]["data"]);
}
