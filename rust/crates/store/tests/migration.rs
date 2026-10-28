mod support;
use agentkit_store::*;
use serde_json::json;
use std::path::Path;
use support::*;

fn version_one(path: &Path) {
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE;
        BEGIN;
        ALTER TABLE events RENAME TO events_v2;
        CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, seq INTEGER NOT NULL CHECK(seq>=0), event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));
        INSERT INTO events SELECT task_id,seq,event_id,record FROM events_v2;
        DROP TABLE events_v2;
        DROP TABLE tool_executions;
        DROP TABLE agent_inputs;
        DELETE FROM counters WHERE name='input_order';
        PRAGMA user_version=1;
        COMMIT;").unwrap();
}

fn records(path: &Path) -> Vec<(String, Vec<String>)> {
    let db = rusqlite::Connection::open(path).unwrap();
    [
        "tasks",
        "attempts",
        "leases",
        "events",
        "outbox",
        "chats",
        "messages",
        "proposals",
        "receipts",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = db
            .prepare(&format!("SELECT record FROM {table} ORDER BY record"))
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        (table.into(), rows)
    })
    .collect()
}

#[tokio::test]
async fn exact_version_one_migrates_preserving_records_and_live_fencing_authority() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v1.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    store
        .create_chat("chat", None, json!({"kept":true}), 1000)
        .await
        .unwrap();
    store
        .submit_turn(
            task("run", "chat"),
            vec![message("message", "chat", Some("run"))],
        )
        .await
        .unwrap();
    let lease = store.claim(claim(1000)).await.unwrap().unwrap().lease;
    store
        .append_events(
            lease.fence(),
            vec![event(0, "zero"), event(7, "seven")],
            1001,
        )
        .await
        .unwrap();
    store
        .set_setting("test", "saved", json!({"kept":true}))
        .await
        .unwrap();
    drop(store);
    version_one(&path);
    let before = records(&path);
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(records(&path), before);
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        agentkit_store::SCHEMA_VERSION
    );
    drop(db);
    assert_eq!(
        reopened.get_task("run").await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    assert_eq!(
        reopened.get_setting("test", "saved").await.unwrap(),
        Some(json!({"kept":true}))
    );
    let mut fractional = event(8, "fractional");
    fractional["seq"] = json!(7.5);
    reopened
        .append_events(lease.fence(), vec![fractional], 1002)
        .await
        .unwrap();
    let replacement = reopened
        .acquire_lease("run", "external", "external", 1003, 100)
        .await
        .unwrap();
    assert!(replacement.fencing_token > lease.fencing_token);
    drop(reopened);
    let final_open = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        final_open
            .events_after_number("run", Some(7.0), 10)
            .await
            .unwrap()[0]
            .seq,
        7.5
    );
}

#[test]
fn malformed_version_one_is_refused_without_migration_or_persistent_pragma_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("malformed-v1.db");
    drop(SqliteStore::open(&path, 4).unwrap());
    version_one(&path);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP INDEX proposal_action;").unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteStore::open(&path, 4),
        Err(StoreError::ForeignDatabase)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
