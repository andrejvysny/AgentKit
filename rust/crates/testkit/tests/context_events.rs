use agentkit_engine::{ClaimInput, SubmitTask, TaskRepository};
use agentkit_testkit::{InMemoryTaskRepository, context_fixtures::recorded_events};
use serde_json::json;

#[tokio::test]
async fn complete_log_includes_events_after_first_page() {
    let store = InMemoryTaskRepository::new();
    store
        .submit(SubmitTask {
            task_id: "task".into(),
            kind: "test".into(),
            scope_id: "scope".into(),
            payload: json!({}),
            priority: 0,
            now: 1000,
            available_at: None,
            depends_on: vec![],
            parent_task_id: None,
            exclusive_scope: false,
        })
        .await
        .unwrap();
    let claimed = store
        .claim_with_concurrency(
            ClaimInput {
                owner_id: "worker".into(),
                now: 1000,
                ttl_ms: 100,
                kinds: vec![],
                busy_scopes: vec![],
                max_attempts: 3,
            },
            1,
        )
        .await
        .unwrap()
        .unwrap();
    let events = (0..1001)
        .map(|seq| {
            json!({"type":"context.cache_break", "seq":seq,
        "eventId":format!("e{seq}"), "timestamp":"2026-10-22T12:00:00.000Z",
        "contractVersion":"0.7.0", "data":{}})
        })
        .collect();
    store
        .append_events(claimed.lease.fence(), events, 1000)
        .await
        .unwrap();
    let captured = recorded_events(&store, "task").await.unwrap();
    assert_eq!(captured.len(), 1001);
    assert_eq!(captured.last().unwrap().seq, 1000.0);
}
