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
        connection.pragma_update(None, "user_version", 3).unwrap();
    }
    let before = std::fs::read(&newer).unwrap();
    assert!(matches!(
        SqliteStore::open(&newer, 2),
        Err(StoreError::UnsupportedVersion(3))
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
