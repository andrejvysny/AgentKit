mod support;
use agentkit_store::*;
use support::*;

#[tokio::test]
async fn explicit_lease_replaces_authority_without_changing_task_or_attempt_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("leases.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let first = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let replacement = store
        .acquire_lease("run", "external-attempt", "external-owner", 1001, 100)
        .await
        .unwrap();
    assert!(replacement.fencing_token > first.fencing_token);
    assert_ne!(replacement.lease_token, first.lease_token);
    assert!(matches!(
        store
            .append_events(first.fence(), vec![event(0, "stale")], 1002)
            .await,
        Err(StoreError::LeaseLost(_))
    ));
    assert_eq!(store.attempts("run").await.unwrap().len(), 1);
    assert_eq!(
        store.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    store
        .append_events(replacement.fence(), vec![event(0, "fresh")], 1002)
        .await
        .unwrap();
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let after_reopen = reopened
        .acquire_lease("run", &first.attempt_id, "worker", 1003, 100)
        .await
        .unwrap();
    assert!(after_reopen.fencing_token > replacement.fencing_token);
    finish(&reopened, after_reopen, TaskStatus::Completed, 1004).await;
    let late = reopened
        .acquire_lease("run", "late-verification", "verifier", 1005, 100)
        .await
        .unwrap();
    reopened
        .append_events(late.fence(), vec![event(1, "late-verification")], 1006)
        .await
        .unwrap();
    assert_eq!(reopened.events("run", None, 10).await.unwrap().len(), 2);
    assert_eq!(
        reopened.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(reopened.attempts("run").await.unwrap().len(), 1);
    assert!(matches!(
        reopened.check_dispatch(late.fence(), 1006).await,
        Err(StoreError::LeaseLost(_))
    ));
    assert!(matches!(
        reopened
            .acquire_lease("run", "bad", "worker", 1007, 0)
            .await,
        Err(StoreError::InvalidInput(_))
    ));
    reopened
        .append_events(late.fence(), vec![event(2, "still-live")], 1008)
        .await
        .unwrap();
}
