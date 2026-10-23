use super::*;

#[tokio::test]
async fn cancelling_completed_fanout_parent_cancels_running_children_and_queued_join() {
    let mut environment = environment(SqliteStore::memory(16).unwrap(), None);
    environment.host.submit(parent_task(false)).await.unwrap();
    let worker = environment.host.start_worker();
    next(&mut environment.parent).await;
    next(&mut environment.leaves).await;
    next(&mut environment.leaves).await;
    assert_eq!(
        terminal(environment.host.store(), "parent").await.status,
        TaskStatus::Completed
    );
    environment.host.cancel("parent").await.unwrap();
    let join = terminal(environment.host.store(), "parent-join").await;
    let first = terminal(environment.host.store(), "parent-leaf-0").await;
    let second = terminal(environment.host.store(), "parent-leaf-1").await;
    worker.shutdown().await.unwrap();
    assert_eq!(join.status, TaskStatus::Cancelled);
    assert_eq!(join.attempt_count, 0);
    assert_eq!(first.status, TaskStatus::Cancelled);
    assert_eq!(second.status, TaskStatus::Cancelled);
    assert_eq!(environment.join_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        environment
            .host
            .store()
            .get_task("parent")
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Completed
    );
    assert_stream(
        environment.host.store(),
        "parent-leaf-0",
        &["demo.leaf.started"],
    )
    .await;
}

#[tokio::test]
async fn explicit_generic_resubmit_returns_original_without_reexecution_or_payload_rewrite() {
    let environment = environment(SqliteStore::memory(16).unwrap(), None);
    let mut input = task("once", "demo.join");
    input.parent_task_id = None;
    input.payload = json!({"text":"original"});
    environment.host.submit(input.clone()).await.unwrap();
    let worker = environment.host.start_worker();
    terminal(environment.host.store(), "once").await;
    input.payload = json!({"text":"different"});
    let again = environment.host.submit(input.clone()).await.unwrap();
    assert_eq!(again.task_id, "once");
    let task = again.status().await.unwrap().unwrap();
    assert_eq!(task.payload, json!({"text":"original"}));
    assert_eq!(task.attempt_count, 1);
    assert_eq!(environment.join_calls.load(Ordering::SeqCst), 1);
    assert!(!environment.host.run_once().await.unwrap());
    input.scope_id = "foreign-scope".into();
    assert!(matches!(
        environment.host.submit(input).await,
        Err(HostError::Store(StoreError::Duplicate(_)))
    ));
    worker.shutdown().await.unwrap();
    assert_eq!(environment.join_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_kind_queues_then_fails_once_without_dead_letter_or_worker_failure() {
    for empty in [false, true] {
        let environment = environment(SqliteStore::memory(16).unwrap(), None);
        let host = if empty {
            AgentHost::new(
                environment.host.store().clone(),
                HashMap::new(),
                environment.host.runtime().clone(),
                HostOptions::default(),
            )
            .unwrap()
        } else {
            environment.host.clone()
        };
        let mut input = task("orphan", "nobody.registered");
        input.parent_task_id = None;
        let handle = host.submit(input).await.unwrap();
        assert_eq!(
            handle.status().await.unwrap().unwrap().status,
            TaskStatus::Queued
        );
        let worker = host.start_worker();
        let record = terminal(host.store(), "orphan").await;
        worker.shutdown().await.unwrap();
        assert_eq!(record.status, TaskStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("executor_not_found: task executor not registered: nobody.registered")
        );
        assert_eq!(record.attempt_count, 1);
        assert_eq!(record.poison_count, 0);
        assert!(!record.retry_pending);
        assert!(record.dead_lettered_at.is_none());
        assert!(record.dead_letter_reason.is_none());
        assert_eq!(
            host.store().attempts("orphan").await.unwrap()[0].status,
            "failed"
        );
        assert!(!host.run_once().await.unwrap());
    }
}
