mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn claim_is_atomic_and_scope_exclusive() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("a", "same")).await.unwrap();
    store.submit(task("b", "same")).await.unwrap();
    let (first, second) = tokio::join!(store.claim(claim(1000)), store.claim(claim(1000)));
    assert_eq!(
        usize::from(first.unwrap().is_some()) + usize::from(second.unwrap().is_some()),
        1
    );
    assert_eq!(store.next_seq("a").await.unwrap(), 0);
    assert!(store.events("a", None, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn append_preserves_increasing_gaps_and_rejected_batches_leave_no_prefix() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("a", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    assert!(
        store
            .append_events(
                lease.fence(),
                vec![event(0, "same"), event(2, "same")],
                1001
            )
            .await
            .is_err()
    );
    assert_eq!(store.next_seq("a").await.unwrap(), 0);
    let wire = event(3, "first");
    store
        .append_events(lease.fence(), vec![wire.clone(), event(7, "second")], 1001)
        .await
        .unwrap();
    assert_eq!(store.events("a", None, 1).await.unwrap()[0].event, wire);
    assert_eq!(store.events("a", Some(3), 10).await.unwrap()[0].seq, 7.0);
    for events in [
        vec![event(9, "prefix"), event(9, "repeat")],
        vec![event(9, "prefix"), event(6, "regression")],
    ] {
        assert!(
            store
                .append_events(lease.fence(), events, 1001)
                .await
                .is_err()
        );
        assert_eq!(store.next_seq("a").await.unwrap(), 8);
    }
    let event_outbox = store
        .outbox()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.topic == "task.events")
        .count();
    assert_eq!(event_outbox, 2);
    finish(&store, lease, TaskStatus::Completed, 1002).await;
    assert_eq!(store.events("a", None, 100).await.unwrap().len(), 2);
}

#[tokio::test]
async fn generic_number_sequences_accept_fractional_and_large_values() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("a", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let mut float = event(0, "float");
    float["seq"] = json!(0.0);
    store
        .append_events(lease.fence(), vec![float], 1001)
        .await
        .unwrap();
    let mut fraction = event(1, "fraction");
    fraction["seq"] = json!(1.5);
    store
        .append_events(lease.fence(), vec![fraction], 1001)
        .await
        .unwrap();
    assert_eq!(store.next_sequence("a").await.unwrap(), 2.5);
    assert!(store.next_seq("a").await.is_err());
    let mut large = event(1, "large");
    large["seq"] = json!(1e100);
    store
        .append_events(lease.fence(), vec![large], 1001)
        .await
        .unwrap();
    assert_eq!(store.next_sequence("a").await.unwrap(), 1e100);
}

#[tokio::test]
async fn stale_worker_cannot_append_project_or_settle_after_recovery() {
    let store = SqliteStore::memory(4).unwrap();
    store.create_chat("s", None, json!({}), 1000).await.unwrap();
    store.submit(task("a", "s")).await.unwrap();
    let first = store.claim(claim(1000)).await.unwrap().unwrap();
    let recovered = store
        .recover(1100, 3, 0, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(recovered[0].status, TaskStatus::Queued);
    assert_eq!(recovered[0].poison_count, 1);
    let second = store.claim(claim(1100)).await.unwrap().unwrap();
    assert!(second.lease.fencing_token > first.lease.fencing_token);
    assert!(matches!(
        store
            .append_events(first.lease.fence(), vec![event(0, "old")], 1101)
            .await,
        Err(StoreError::LeaseLost(_))
    ));
    assert!(
        store
            .append_projected(
                first.lease.fence(),
                vec![],
                vec![message("z", "s", Some("a"))],
                1101
            )
            .await
            .is_err()
    );
    assert!(
        store
            .settle(SettleTask {
                fence: first.lease.fence(),
                now: 1101,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![]
            })
            .await
            .is_err()
    );
    assert!(store.get_message("z").await.unwrap().is_none());
    assert_eq!(
        store.get_task("a").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    let attempts = store.attempts("a").await.unwrap();
    assert_eq!(attempts[0].status, "abandoned");
    assert_eq!(attempts[1].status, "running");
}

#[tokio::test]
async fn cancellation_holds_lease_until_terminal_evidence() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("a", "s")).await.unwrap();
    store.submit(task("b", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let pending = store.cancel_task("a", 1001).await.unwrap();
    assert!(pending.cancel_requested);
    assert_eq!(pending.status, TaskStatus::Running);
    assert!(store.claim(claim(1001)).await.unwrap().is_none());
    store
        .append_events(lease.fence(), vec![event(0, "cancel-evidence")], 1002)
        .await
        .unwrap();
    finish(&store, lease, TaskStatus::Completed, 1003).await;
    assert_eq!(
        store.get_task("a").await.unwrap().unwrap().status,
        TaskStatus::Cancelled
    );
    assert_eq!(
        store
            .claim(claim(1003))
            .await
            .unwrap()
            .unwrap()
            .task
            .task_id,
        "b"
    );
}

#[tokio::test]
async fn priorities_dependencies_and_dead_letters_survive_recovery() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("base", "a")).await.unwrap();
    let mut dependent = task("dependent", "b");
    dependent.depends_on = vec!["base".into()];
    dependent.priority = 100;
    store.submit(dependent).await.unwrap();
    let first = store.claim(claim(1000)).await.unwrap().unwrap();
    assert_eq!(first.task.task_id, "base");
    finish(&store, first.lease, TaskStatus::Failed, 1001).await;
    assert!(store.claim(claim(1002)).await.unwrap().is_none());
    assert_eq!(
        store
            .get_task("dependent")
            .await
            .unwrap()
            .unwrap()
            .error
            .as_deref(),
        Some("dependency_failed: base")
    );
    store.submit(task("poison", "p")).await.unwrap();
    store.claim(claim(1000)).await.unwrap().unwrap();
    let recovered = store
        .recover(1100, 1, 0, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(
        recovered[0].dead_letter_reason.as_deref(),
        Some("attempts_exhausted")
    );
    assert!(store.claim(claim(1101)).await.unwrap().is_none());
}

#[tokio::test]
async fn manual_recovery_requires_resume_and_invalid_retry_options_change_nothing() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("a", "s")).await.unwrap();
    store.claim(claim(1000)).await.unwrap();
    assert!(
        store
            .recover(1100, 0, 0, RecoveryMode::Automatic)
            .await
            .is_err()
    );
    assert_eq!(
        store.get_task("a").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    store
        .recover(1100, 3, 50, RecoveryMode::Manual)
        .await
        .unwrap();
    assert!(store.claim(claim(1200)).await.unwrap().is_none());
    store.resume_task("a", 1200).await.unwrap();
    assert!(store.claim(claim(1200)).await.unwrap().is_some());
}

#[tokio::test]
async fn submit_and_project_are_atomic_idempotent_and_scope_bound() {
    let store = SqliteStore::memory(4).unwrap();
    store.create_chat("s", None, json!({}), 1000).await.unwrap();
    assert!(
        store
            .submit_turn(task("a", "s"), vec![message("bad", "foreign", Some("a"))])
            .await
            .is_err()
    );
    assert!(store.get_task("a").await.unwrap().is_none());
    let mut placeholder = message("placeholder", "s", Some("a"));
    placeholder.role = "assistant".into();
    placeholder.metadata = json!({"placeholder":true});
    store
        .submit_turn(
            task("a", "s"),
            vec![message("user", "s", Some("a")), placeholder],
        )
        .await
        .unwrap();
    store.submit_turn(task("a", "s"), vec![]).await.unwrap();
    assert_eq!(store.messages("s", true).await.unwrap().len(), 2);
    let mut changed = task("a", "s");
    changed.payload = json!({"different":true});
    assert!(store.submit_turn(changed, vec![]).await.is_err());
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let mut final_message = message("placeholder", "s", Some("a"));
    final_message.role = "assistant".into();
    final_message.content = json!("done");
    store
        .append_projected(
            lease.fence(),
            vec![event(0, "final")],
            vec![final_message.clone()],
            1001,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get_message("placeholder")
            .await
            .unwrap()
            .unwrap()
            .content,
        json!("done")
    );
    assert!(
        store
            .append_projected(
                lease.fence(),
                vec![event(1, "duplicate-final")],
                vec![final_message],
                1002
            )
            .await
            .is_err()
    );
    assert_eq!(store.next_seq("a").await.unwrap(), 1);
}

#[tokio::test]
async fn child_creation_requires_live_parent_and_progress_never_changes_events() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("parent", "scope")).await.unwrap();
    let claimed = store.claim(claim(1000)).await.unwrap().unwrap();
    let mut child = task("child", "other");
    child.parent_task_id = Some("parent".into());
    store
        .submit_child(claimed.lease.fence(), child.clone(), 1001)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_task("child")
            .await
            .unwrap()
            .unwrap()
            .parent_task_id
            .as_deref(),
        Some("parent")
    );
    store
        .update_progress(claimed.lease.fence(), json!({"done":1}), 1002)
        .await
        .unwrap();
    assert!(store.events("parent", None, 100).await.unwrap().is_empty());
    store.cancel_task("parent", 1003).await.unwrap();
    child.task_id = "refused".into();
    assert!(
        store
            .submit_child(claimed.lease.fence(), child, 1004)
            .await
            .is_err()
    );
    assert!(store.get_task("refused").await.unwrap().is_none());
}

#[tokio::test]
async fn private_runtime_state_compare_and_set_rejects_stale_checkpoint() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("task", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .checkpoint_runtime_state(lease.fence(), None, json!({"count":1}), 1001)
        .await
        .unwrap();
    assert!(
        store
            .checkpoint_runtime_state(lease.fence(), None, json!({"count":0}), 1002)
            .await
            .is_err()
    );
    assert_eq!(
        store.get_task("task").await.unwrap().unwrap().runtime_state,
        Some(json!({"count":1}))
    );
}

#[tokio::test]
async fn dispatch_authority_requires_live_uncancelled_current_attempt() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let first = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store.check_dispatch(first.fence(), 1099).await.unwrap();
    assert!(matches!(
        store.check_dispatch(first.fence(), 1100).await,
        Err(StoreError::LeaseLost(_))
    ));
    store
        .recover(1100, 3, 0, RecoveryMode::Manual)
        .await
        .unwrap();
    store.resume_task("run", 1100).await.unwrap();
    let second = store.claim(claim(1100)).await.unwrap().unwrap().lease;
    assert!(matches!(
        store.check_dispatch(first.fence(), 1101).await,
        Err(StoreError::LeaseLost(_))
    ));
    store.check_dispatch(second.fence(), 1101).await.unwrap();
    store.cancel_task("run", 1102).await.unwrap();
    assert!(matches!(
        store.check_dispatch(second.fence(), 1103).await,
        Err(StoreError::LeaseLost(_))
    ));
}
