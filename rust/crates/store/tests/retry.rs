mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn retry_validation_and_stale_fences_cannot_release_current_ownership() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("run", "s")).await.unwrap();
    let first = store.claim(claim(1000)).await.unwrap().unwrap();
    let retry = RetryTask {
        fence: first.lease.fence(),
        now: 1001,
        available_at: 1000,
        max_attempts: 3,
        error: "network: temporary".into(),
    };
    assert!(store.retry_task(retry.clone()).await.is_err());
    store
        .update_progress(first.lease.fence(), json!({"stillOwned":true}), 1002)
        .await
        .unwrap();
    assert_eq!(store.attempts("run").await.unwrap()[0].status, "running");
    store
        .retry_task(RetryTask {
            available_at: 1010,
            ..retry
        })
        .await
        .unwrap();
    assert!(store.renew(first.lease.fence(), 1003, 100).await.is_err());
    let second = store.claim(claim(1010)).await.unwrap().unwrap();
    assert!(second.lease.fencing_token > first.lease.fencing_token);
    assert!(
        store
            .retry_task(RetryTask {
                fence: first.lease.fence(),
                now: 1011,
                available_at: 2000,
                max_attempts: 3,
                error: "stale".into()
            })
            .await
            .is_err()
    );
    assert!(!store.get_task("run").await.unwrap().unwrap().retry_pending);
}

#[tokio::test]
async fn manual_recovery_parks_even_attempt_exhausted_work_without_auto_dead_letter() {
    let store = SqliteStore::memory(4).unwrap();
    store.submit(task("run", "s")).await.unwrap();
    store.claim(claim(1000)).await.unwrap();
    let recovered = store
        .recover(1100, 1, 0, RecoveryMode::Manual)
        .await
        .unwrap();
    assert_eq!(recovered[0].status, TaskStatus::Interrupted);
    assert!(recovered[0].dead_lettered_at.is_none());
    assert_eq!(recovered[0].poison_count, 1);
}

#[tokio::test]
async fn settings_compare_and_set_accepts_one_winner_without_lost_update() {
    let store = SqliteStore::memory(4).unwrap();
    let other = store.clone();
    let (first, second) = tokio::join!(
        store.compare_and_set_setting("mcp", "configs", None, json!(["first"])),
        other.compare_and_set_setting("mcp", "configs", None, json!(["second"]))
    );
    assert_ne!(first.unwrap(), second.unwrap());
    let current = store.get_setting("mcp", "configs").await.unwrap().unwrap();
    assert!(
        store
            .compare_and_set_setting("mcp", "configs", Some(current), json!(["merged"]))
            .await
            .unwrap()
    );
    assert!(
        !store
            .compare_and_set_setting("mcp", "configs", None, json!([]))
            .await
            .unwrap()
    );
    assert_eq!(
        store.get_setting("mcp", "configs").await.unwrap(),
        Some(json!(["merged"]))
    );
}
