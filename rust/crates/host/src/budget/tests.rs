use super::*;
use agentkit_store::{ClaimInput, RecoveryMode, SqliteStore, SubmitTask};
use serde_json::json;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio_util::sync::CancellationToken;

struct Clock(AtomicI64);
impl crate::Runtime for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
impl Runtime for Clock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst).max(0) as u64
    }
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", uuid::Uuid::new_v4())
    }
    fn timestamp(&self) -> String {
        "2026-10-22T12:00:00Z".into()
    }
}

fn claim(now: i64) -> ClaimInput {
    ClaimInput {
        owner_id: "worker".into(),
        now,
        ttl_ms: 100,
        kinds: vec![],
        busy_scopes: vec![],
        max_attempts: 3,
    }
}

async fn context(store: SqliteStore, clock: Arc<Clock>, new_task: bool) -> ExecutionContext {
    if new_task {
        store
            .submit(SubmitTask {
                task_id: "run".into(),
                kind: "test".into(),
                scope_id: "s".into(),
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
    }
    let claimed = store
        .claim(claim(crate::Runtime::now_ms(clock.as_ref())))
        .await
        .unwrap()
        .unwrap();
    ExecutionContext {
        task: claimed.task,
        attempt_id: claimed.lease.attempt_id.clone(),
        cancellation: CancellationToken::new(),
        interruption: CancellationToken::new(),
        suspension: Arc::new(Mutex::new(None)),
        store,
        fence: claimed.lease.fence(),
        runtime: clock,
        event_gate: Arc::new(Mutex::new(())),
    }
}

#[tokio::test]
async fn reservation_survives_crash_before_dispatch_and_retry_keeps_deadline() {
    let directory = std::env::temp_dir().join(format!("agentkit-budget-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("store.db");
    let store = SqliteStore::open(&path, 2).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let first = context(store.clone(), clock.clone(), true).await;
    let limits = BudgetLimits {
        provider_requests: Some(1),
        overall_ms: Some(10000),
        retries: Some(1),
        ..Default::default()
    };
    let budget = restore_execution_budget(&first, limits.clone(), clock.clone())
        .await
        .unwrap();
    budget.consume("providerRequests").await.unwrap();
    assert_eq!(
        store
            .get_task("run")
            .await
            .unwrap()
            .unwrap()
            .runtime_state
            .unwrap()["snapshot"]["providerRequests"],
        1
    );
    drop(budget);
    drop(first);
    drop(store);
    let reopened = SqliteStore::open(&path, 2).unwrap();
    clock.0.store(1100, Ordering::SeqCst);
    reopened
        .recover(1100, 3, 0, RecoveryMode::Automatic)
        .await
        .unwrap();
    let second = context(reopened.clone(), clock.clone(), false).await;
    let budget = restore_execution_budget(&second, limits, clock.clone())
        .await
        .unwrap();
    assert_eq!(budget.snapshot().provider_requests, 1);
    assert_eq!(budget.snapshot().retries, 1);
    assert_eq!(budget.remaining_ms(), Some(9900));
    assert!(budget.consume("providerRequests").await.is_err());
    let again = restore_execution_budget(&second, budget.limits.clone(), clock)
        .await
        .unwrap();
    assert_eq!(again.snapshot().retries, 1);
    drop(again);
    drop(budget);
    drop(second);
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn failed_checkpoint_blocks_dispatch_and_budget_remains_failed() {
    let store = SqliteStore::memory(2).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let first = context(store.clone(), clock.clone(), true).await;
    let budget = restore_execution_budget(&first, BudgetLimits::default(), clock.clone())
        .await
        .unwrap();
    clock.0.store(1100, Ordering::SeqCst);
    store
        .recover(1100, 3, 0, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert!(budget.consume("providerRequests").await.is_err());
    assert!(budget.check().is_err());
    assert_eq!(
        store
            .get_task("run")
            .await
            .unwrap()
            .unwrap()
            .runtime_state
            .unwrap()["snapshot"]["providerRequests"],
        0
    );
}

#[tokio::test]
async fn malformed_missing_changed_and_newer_budgets_fail_closed() {
    assert!(parse_budget_limits(Some(&json!({"inventedLimit":1}))).is_err());
    let store = SqliteStore::memory(2).unwrap();
    let clock = Arc::new(Clock(AtomicI64::new(1000)));
    let first = context(store.clone(), clock.clone(), true).await;
    let budget = restore_execution_budget(
        &first,
        BudgetLimits {
            provider_requests: Some(1),
            ..Default::default()
        },
        clock.clone(),
    )
    .await
    .unwrap();
    assert!(
        restore_execution_budget(
            &first,
            BudgetLimits {
                provider_requests: Some(2),
                ..Default::default()
            },
            clock.clone()
        )
        .await
        .is_err()
    );
    let previous = store.get_task("run").await.unwrap().unwrap().runtime_state;
    let mut newer = previous.clone().unwrap();
    newer["version"] = json!(2);
    store
        .checkpoint_runtime_state(first.fence.clone(), previous, newer, 1001)
        .await
        .unwrap();
    assert!(
        restore_execution_budget(&first, budget.limits.clone(), clock.clone())
            .await
            .is_err()
    );
    let other = SqliteStore::memory(2).unwrap();
    let _uninitialized = context(other.clone(), clock.clone(), true).await;
    clock.0.store(1100, Ordering::SeqCst);
    other
        .recover(1100, 3, 0, RecoveryMode::Automatic)
        .await
        .unwrap();
    let second = context(other, clock.clone(), false).await;
    assert!(
        restore_execution_budget(&second, BudgetLimits::default(), clock)
            .await
            .is_err()
    );
}
