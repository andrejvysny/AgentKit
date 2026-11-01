mod support;
use agentkit_store::*;
use serde_json::json;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use support::*;

#[tokio::test]
async fn crash_process_worker() {
    let Ok(path) = std::env::var("AGENTKIT_STORE_CRASH_PATH") else {
        return;
    };
    let store = SqliteStore::open(path, 2).unwrap();
    store.submit(task("crashed", "s")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .append_events(lease.fence(), vec![event(0, "before-crash")], 1001)
        .await
        .unwrap();
    std::fs::write(
        std::env::var("AGENTKIT_STORE_READY_PATH").unwrap(),
        b"ready",
    )
    .unwrap();
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn killed_process_reopens_with_durable_recovery_before_new_claim() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let ready = directory.path().join("ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_process_worker", "--nocapture"])
        .env("AGENTKIT_STORE_CRASH_PATH", &path)
        .env("AGENTKIT_STORE_READY_PATH", &ready)
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !ready.exists() {
        let _ = child.kill();
        panic!("crash worker did not become ready");
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let store = SqliteStore::open(&path, 2).unwrap();
    assert_eq!(
        store.get_task("crashed").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(
        store.events("crashed", None, 10).await.unwrap()[0].event,
        event(0, "before-crash")
    );
    let recovered = store
        .recover(1100, 3, 50, RecoveryMode::Automatic)
        .await
        .unwrap();
    assert_eq!(recovered[0].available_at, 1150);
    assert!(store.claim(claim(1149)).await.unwrap().is_none());
    drop(store);
    let reopened = SqliteStore::open(&path, 2).unwrap();
    let task = reopened.get_task("crashed").await.unwrap().unwrap();
    assert_eq!(task.status, TaskStatus::Queued);
    assert_eq!(task.poison_count, 1);
    assert_eq!(
        reopened.attempts("crashed").await.unwrap()[0].status,
        "abandoned"
    );
    assert!(
        reopened
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|record| record.payload["type"] == "task.recovered")
    );
    assert_eq!(
        reopened
            .claim(claim(1150))
            .await
            .unwrap()
            .unwrap()
            .task
            .attempt_count,
        2
    );
}

#[tokio::test]
async fn ownership_is_exclusive_and_released_on_last_clone_drop() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.db");
    let store = SqliteStore::open(&path, 2).unwrap();
    let clone = store.clone();
    #[cfg(unix)]
    {
        let alias = directory.path().join("hard-link.db");
        std::fs::hard_link(&path, &alias).unwrap();
        assert!(matches!(
            SqliteStore::open(&alias, 2),
            Err(StoreError::AlreadyOwned)
        ));
    }
    assert!(matches!(
        SqliteStore::open(&path, 2),
        Err(StoreError::AlreadyOwned)
    ));
    drop(store);
    assert!(matches!(
        SqliteStore::open(&path, 2),
        Err(StoreError::AlreadyOwned)
    ));
    clone
        .set_setting("test", "saved", json!({"a":1}))
        .await
        .unwrap();
    drop(clone);
    let reopened = SqliteStore::open(&path, 2).unwrap();
    assert_eq!(
        reopened.get_setting("test", "saved").await.unwrap(),
        Some(json!({"a":1}))
    );
}

#[test]
fn foreign_and_newer_databases_are_refused_without_database_writes() {
    let directory = tempfile::tempdir().unwrap();
    let foreign = directory.path().join("typescript.db");
    {
        let connection = rusqlite::Connection::open(&foreign).unwrap();
        connection
            .execute_batch("CREATE TABLE chats(id TEXT);PRAGMA user_version=8;")
            .unwrap();
    }
    let before = std::fs::read(&foreign).unwrap();
    assert!(matches!(
        SqliteStore::open(&foreign, 2),
        Err(StoreError::ForeignDatabase)
    ));
    assert_eq!(std::fs::read(&foreign).unwrap(), before);
    let newer = directory.path().join("newer.db");
    drop(SqliteStore::open(&newer, 2).unwrap());
    {
        let connection = rusqlite::Connection::open(&newer).unwrap();
        connection
            .pragma_update(None, "user_version", agentkit_store::SCHEMA_VERSION + 1)
            .unwrap();
    }
    let before = std::fs::read(&newer).unwrap();
    assert!(matches!(
        SqliteStore::open(&newer, 2),
        Err(StoreError::UnsupportedVersion(v)) if v == agentkit_store::SCHEMA_VERSION + 1
    ));
    assert_eq!(std::fs::read(&newer).unwrap(), before);
}

#[test]
fn malformed_same_version_schema_is_refused_before_persistent_pragmas() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("malformed.db");
    drop(SqliteStore::open(&path, 2).unwrap());
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=DELETE; DROP INDEX proposal_action;")
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteStore::open(&path, 2),
        Err(StoreError::ForeignDatabase)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

#[tokio::test]
async fn reopened_handle_enforces_persisted_fence_without_partial_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fence.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    // A second live handle over the same file is refused outright.
    assert!(matches!(
        SqliteStore::open(&path, 4),
        Err(StoreError::AlreadyOwned)
    ));
    // Lease authority lives in the database: the same fence keeps working
    // on a fresh handle, and expiry is enforced there too.
    reopened.verify_fence(&lease.fence(), 1001).await.unwrap();
    reopened
        .append_events(lease.fence(), vec![event(0, "after-reopen")], 1001)
        .await
        .unwrap();
    assert!(matches!(
        reopened
            .append_events(lease.fence(), vec![event(1, "expired")], 1100)
            .await,
        Err(StoreError::LeaseLost(id)) if id == "run"
    ));
    // New authority minted by the fresh handle fences the old writer out.
    let replacement = reopened
        .acquire_lease("run", &lease.attempt_id, "worker-b", 1150, 100)
        .await
        .unwrap();
    assert!(replacement.fencing_token > lease.fencing_token);
    assert!(matches!(
        reopened
            .append_events(lease.fence(), vec![event(1, "zombie")], 1160)
            .await,
        Err(StoreError::LeaseLost(id)) if id == "run"
    ));
    assert!(matches!(
        reopened
            .settle(SettleTask {
                fence: lease.fence(),
                now: 1160,
                status: TaskStatus::Completed,
                error: None,
                events: vec![event(1, "zombie-settle")],
                messages: vec![],
            })
            .await,
        Err(StoreError::LeaseLost(id)) if id == "run"
    ));
    // Zero partial writes: only the committed event survives, no zombie
    // payload reached the outbox, and task/attempt state is untouched.
    let events = reopened.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event(0, "after-reopen"));
    assert!(
        !reopened
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|record| record.payload.to_string().contains("zombie"))
    );
    let attempts = reopened.attempts("run").await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "running");
    assert_eq!(
        reopened.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    // The refused batches advanced no sequence watermark: the fresh fence
    // still owns the run and continues exactly at seq 1.
    reopened
        .append_events(
            replacement.fence(),
            vec![event(1, "after-replacement")],
            1160,
        )
        .await
        .unwrap();
    finish(&reopened, replacement, TaskStatus::Completed, 1170).await;
    drop(reopened);
    let final_handle = SqliteStore::open(&path, 4).unwrap();
    let events = final_handle.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event, event(1, "after-replacement"));
}

#[tokio::test]
async fn invalid_envelope_after_valid_prefix_fails_typed_and_commits_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollback.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store.submit(task("run", "scope")).await.unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    let mut invalid = event(1, "invalid");
    invalid["eventId"] = json!(1);
    let error = store
        .append_events(lease.fence(), vec![event(0, "prefix"), invalid], 1001)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidInput(message)
            if message.starts_with("invalid task event envelope:")),
        "{error}"
    );
    // The valid first event of the batch committed nothing anywhere.
    assert!(store.events("run", None, 10).await.unwrap().is_empty());
    assert!(
        !store
            .outbox()
            .await
            .unwrap()
            .iter()
            .any(|record| record.payload["eventId"] == json!("prefix"))
    );
    // No watermark leaked from the rolled-back prefix: a fully valid batch
    // starting again at seq 0 still lands.
    store
        .append_events(lease.fence(), vec![event(0, "retried")], 1001)
        .await
        .unwrap();
    let events = store.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event(0, "retried"));
    drop(store);
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let events = reopened.events("run", None, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, event(0, "retried"));
}
