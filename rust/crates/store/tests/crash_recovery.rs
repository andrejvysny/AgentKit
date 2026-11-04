//! Crash recovery must never re-run finished work, and the claim/lease
//! bookkeeping around it must stay honest.
mod support;
use agentkit_store::*;
use support::*;

#[tokio::test]
async fn recover_ignores_a_stale_lease_on_a_settled_task() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("done", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    finish(&store, lease, TaskStatus::Completed, 1001).await;
    // A trusted low-level lease on a terminal task: residue, not live work.
    store
        .acquire_lease("done", "late", "verifier", 1002, 10)
        .await
        .unwrap();
    let recovered = store
        .recover(5000, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert!(recovered.is_empty());
    let task = store.get_task("done").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert_eq!(task.poison_count, 0);
    assert!(store.get_lease("done").await.unwrap().is_none());
}

#[tokio::test]
async fn recover_requeues_a_running_task_that_lost_its_lease_row() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("orphan", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store.release_lease(&lease.lease_token).await.unwrap();
    assert_eq!(
        store.get_task("orphan").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    // Inside the grace window a lease-less task is a port-path transition
    // in flight, not an orphan.
    let fresh = store
        .recover(2000, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert!(fresh.is_empty());
    assert_eq!(
        store.get_task("orphan").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    let recovered = store
        .recover(1000 + 600_001, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].status, TaskStatus::Queued);
    assert_eq!(recovered[0].poison_count, 1);
    assert_eq!(
        store.attempts("orphan").await.unwrap()[0].status,
        "abandoned"
    );
}

#[tokio::test]
async fn an_expired_lease_left_on_a_requeued_task_is_residue_not_a_crash() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("parked", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    for (from, to) in [
        (TaskStatus::Running, TaskStatus::WaitingApproval),
        (TaskStatus::WaitingApproval, TaskStatus::Queued),
    ] {
        store
            .transition_task(
                "parked",
                vec![from],
                to,
                TaskTransitionPatch::default(),
                None,
                1001,
            )
            .await
            .unwrap();
    }
    assert!(store.get_lease("parked").await.unwrap().is_some());
    let recovered = store
        .recover(5000, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert!(recovered.is_empty());
    let task = store.get_task("parked").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.poison_count, 0);
    assert_eq!(
        store.attempts("parked").await.unwrap()[0].status,
        "abandoned"
    );
    assert!(store.get_lease("parked").await.unwrap().is_none());
    drop(lease);
}

#[tokio::test]
async fn recover_leaves_a_parked_retry_alone() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("retry", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .retry_task(RetryTask {
            fence: lease.fence(),
            now: 1001,
            available_at: 1500,
            max_attempts: 3,
            error: "transient".into(),
        })
        .await
        .unwrap();
    let recovered = store
        .recover(2000, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert!(recovered.is_empty());
    assert!(
        store
            .get_task("retry")
            .await
            .unwrap()
            .unwrap()
            .retry_pending
    );
}

#[tokio::test]
async fn terminal_transition_drops_the_lease_but_dead_letter_marker_keeps_it() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("a", "sa")).await.unwrap();
    store.submit(task("b", "sb")).await.unwrap();
    let first = store.claim(claim(1000)).await.unwrap().unwrap();
    let second = store.claim(claim(1000)).await.unwrap().unwrap();
    let (a, b) = if first.task.task_id == "a" {
        (first, second)
    } else {
        (second, first)
    };
    store
        .transition_task(
            "a",
            vec![TaskStatus::Running],
            TaskStatus::Completed,
            TaskTransitionPatch::default(),
            Some(a.lease.lease_token.clone()),
            1001,
        )
        .await
        .unwrap();
    assert!(store.get_lease("a").await.unwrap().is_none());
    store
        .mark_dead_lettered("b", "poison", Some(b.lease.lease_token.clone()), 1002)
        .await
        .unwrap();
    // A marker, not a status change: the live attempt still owns the lease
    // and lands the terminal write with it.
    assert!(store.get_lease("b").await.unwrap().is_some());
    store
        .transition_task(
            "b",
            vec![TaskStatus::Running],
            TaskStatus::Failed,
            TaskTransitionPatch::default(),
            Some(b.lease.lease_token.clone()),
            1003,
        )
        .await
        .unwrap();
    assert!(store.get_lease("b").await.unwrap().is_none());
}

#[tokio::test]
async fn poison_count_overflow_is_refused() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("p", "s")).await.unwrap();
    store
        .transition_task(
            "p",
            vec![TaskStatus::Queued],
            TaskStatus::Running,
            TaskTransitionPatch {
                poison_count: Some(u32::MAX),
                ..Default::default()
            },
            None,
            1001,
        )
        .await
        .unwrap();
    let attempt = store.create_attempt("p", "att", "w", 1002).await.unwrap();
    let error = store
        .end_attempt(&attempt.attempt_id, "abandoned", None, None, 1003)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidInput(message) if message.contains("overflow")),
        "{error:?}"
    );
}

#[tokio::test]
async fn delete_by_scope_refuses_an_interrupted_task() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("i", "scope")).await.unwrap();
    assert!(store.interrupt_task("i", "boot", None, 1001).await.unwrap());
    assert!(matches!(
        store.delete_by_scope("scope").await,
        Err(StoreError::ScopeBusy(_))
    ));
    store.cancel_task("i", 1002).await.unwrap();
    assert_eq!(store.delete_by_scope("scope").await.unwrap(), 1);
}

#[tokio::test]
async fn waiting_approval_blocks_its_scope_but_not_a_concurrency_slot() {
    let store = SqliteStore::memory(2).unwrap();
    store.submit(task("parked", "a")).await.unwrap();
    store.claim(claim(1000)).await.unwrap().unwrap();
    store
        .transition_task(
            "parked",
            vec![TaskStatus::Running],
            TaskStatus::WaitingApproval,
            TaskTransitionPatch::default(),
            None,
            1001,
        )
        .await
        .unwrap();
    store.submit(task("same-scope", "a")).await.unwrap();
    store.submit(task("other-scope", "b")).await.unwrap();
    let claimed = store
        .claim_with_concurrency(claim(1002), 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.task.task_id, "other-scope");
    // The scope itself stays blocked even with unlimited slots.
    finish(&store, claimed.lease, TaskStatus::Completed, 1002).await;
    assert!(store.claim(claim(1003)).await.unwrap().is_none());
}

#[tokio::test]
async fn invalid_aging_is_refused_at_claim() {
    for aging in [
        TaskAging {
            interval_ms: 0,
            bonus: 1,
            max_bonus: 1,
        },
        TaskAging {
            interval_ms: 10,
            bonus: -1,
            max_bonus: 1,
        },
        TaskAging {
            interval_ms: 10,
            bonus: 1,
            max_bonus: -1,
        },
    ] {
        let store = SqliteStore::memory(2).unwrap().with_task_aging(aging);
        store.submit(task("t", "s")).await.unwrap();
        assert!(matches!(
            store.claim(claim(1000)).await,
            Err(StoreError::InvalidInput(_))
        ));
    }
}
