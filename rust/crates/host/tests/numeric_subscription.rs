use agentkit_host::{
    AgentHost, ExecutionContext, HostOptions, HostResult, RunUpdate, SystemRuntime, TaskExecutor,
};
use agentkit_store::{SqliteStore, SubmitTask};
use async_trait::async_trait;
use serde_json::json;
use std::{collections::HashMap, sync::Arc};

struct FractionalEvents;
#[async_trait]
impl TaskExecutor for FractionalEvents {
    async fn execute(&self, _context: ExecutionContext) -> HostResult<()> {
        Ok(())
    }
}

async fn append_events(store: &SqliteStore) {
    let lease = store
        .acquire_lease("numeric-log", "late-events", "test", 1, 1000)
        .await
        .unwrap();
    let events = [-0.5, 0.25, 0.5, 2.5]
        .into_iter()
        .enumerate()
        .map(|(index, seq)| {
            json!({"seq":seq,"eventId":format!("event-{index}"),"type":"custom.progress",
                "timestamp":"opaque timestamp","contractVersion":"custom"})
        })
        .collect();
    store.append_events(lease.fence(), events, 1).await.unwrap();
}

async fn completed_run() -> agentkit_host::RunHandle {
    let store = SqliteStore::memory(16).unwrap();
    let host = AgentHost::new(
        store.clone(),
        HashMap::from([(
            "custom".into(),
            Arc::new(FractionalEvents) as Arc<dyn TaskExecutor>,
        )]),
        Arc::new(SystemRuntime),
        HostOptions::default(),
    )
    .unwrap();
    let run = host
        .submit(SubmitTask {
            task_id: "numeric-log".into(),
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
    append_events(&store).await;
    run
}

#[tokio::test]
async fn native_subscription_uses_exclusive_numeric_cursor_without_losing_events() {
    let run = completed_run().await;
    let mut updates = run.subscribe_after_number(Some(0.25));
    let mut sequences = vec![];
    let mut settled = false;
    while let Some(update) = updates.recv().await {
        match update.unwrap() {
            RunUpdate::Event(event) => sequences.push(event.seq),
            RunUpdate::Settled(_) => settled = true,
        }
    }
    assert_eq!(sequences, [0.5, 2.5]);
    assert!(settled);
}
