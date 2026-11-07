mod support;
use agentkit_store::*;
use serde_json::json;
use support::*;

const TASKS: usize = 240;

/// 4 scopes, 3 root parents; every task past the roots hangs off one root.
async fn fixture(store: &SqliteStore) {
    for scope in 0..4 {
        store
            .create_chat(&format!("s{scope}"), None, json!({}), 1000)
            .await
            .unwrap();
    }
    for i in 0..TASKS {
        let mut input = task(&format!("t{i:03}"), &format!("s{}", i % 4));
        input.now = 1000 + i as i64;
        if i >= 3 {
            input.parent_task_id = Some(format!("t{:03}", i % 3));
        }
        store.submit(input).await.unwrap();
    }
}

fn ids(tasks: &[TaskRecord]) -> Vec<String> {
    tasks.iter().map(|t| t.task_id.clone()).collect()
}

#[tokio::test]
async fn list_queries_match_a_rust_side_filter_of_every_task() {
    let store = SqliteStore::open(":memory:", 4).unwrap();
    fixture(&store).await;
    let all = store.list_tasks(None).await.unwrap();
    assert_eq!(all.len(), TASKS);

    for scope in ["s0", "s3", "missing"] {
        let expected: Vec<_> = all
            .iter()
            .filter(|t| t.scope_id == scope)
            .cloned()
            .collect();
        assert_eq!(
            store.list_tasks(Some(scope.into())).await.unwrap(),
            expected
        );
    }
    for parent in ["t000", "t001", "t002", "t100", "missing"] {
        let mut expected: Vec<_> = all
            .iter()
            .filter(|t| t.parent_task_id.as_deref() == Some(parent))
            .cloned()
            .collect();
        expected.sort_by_key(|t| (t.enqueued_at, t.task_id.clone()));
        let got = store.list_children(parent).await.unwrap();
        assert_eq!(ids(&got), ids(&expected), "children of {parent}");
    }
    assert_eq!(
        store.list_children("t000").await.unwrap().len(),
        TASKS / 3 - 1
    );
}

#[tokio::test]
async fn interrupt_queued_only_touches_queued_work_enqueued_before_the_cutoff() {
    let store = SqliteStore::open(":memory:", 4).unwrap();
    fixture(&store).await;
    // One task leaves `queued`: it must not be interrupted.
    let running = store.claim(claim(2000)).await.unwrap().unwrap().task;
    assert_eq!(running.status, TaskStatus::Running);
    let before = store.list_tasks(None).await.unwrap();
    let expected = before
        .iter()
        .filter(|t| t.status == TaskStatus::Queued && t.enqueued_at < 1100)
        .count();
    assert!(expected > 90, "fixture must exercise the cutoff");

    let count = store.interrupt_queued(1100).await.unwrap();
    let all = store.list_tasks(None).await.unwrap();
    let interrupted: Vec<_> = all
        .iter()
        .filter(|t| t.status == TaskStatus::Interrupted)
        .collect();
    assert_eq!(count, expected);
    assert_eq!(interrupted.len(), expected);
    assert!(interrupted.iter().all(|t| t.enqueued_at < 1100));
    assert_eq!(
        store
            .get_task(&running.task_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        TaskStatus::Running
    );
    let queued = all
        .iter()
        .filter(|t| t.status == TaskStatus::Queued)
        .count();
    assert_eq!(queued, TASKS - 1 - expected);
}

#[tokio::test]
async fn delete_chat_removes_only_that_scope_once_it_is_settled() {
    let store = SqliteStore::open(":memory:", 4).unwrap();
    fixture(&store).await;
    assert!(matches!(
        store.delete_chat("s1").await,
        Err(StoreError::ScopeBusy(_))
    ));
    for task in store.list_tasks(Some("s1".into())).await.unwrap() {
        store.cancel_task(&task.task_id, 3000).await.unwrap();
    }
    store.delete_chat("s1").await.unwrap();
    assert!(
        store
            .list_tasks(Some("s1".into()))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.list_tasks(None).await.unwrap().len(),
        TASKS - TASKS / 4
    );
    assert_eq!(
        store.list_tasks(Some("s2".into())).await.unwrap().len(),
        TASKS / 4
    );
}

fn plan(db: &rusqlite::Connection, sql: &str) -> String {
    let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .map(|row| row.unwrap())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[tokio::test]
async fn hot_task_queries_use_the_new_indexes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plans.db");
    drop(SqliteStore::open(&path, 4).unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    let children = plan(
        &db,
        "SELECT record FROM tasks WHERE json_extract(record, '$.parentTaskId')='x' ORDER BY rowid",
    );
    assert!(children.contains("USING INDEX tasks_parent"), "{children}");
    let claim = plan(
        &db,
        "SELECT record FROM tasks WHERE status IN ('queued','running')",
    );
    assert!(claim.contains("USING INDEX tasks_status"), "{claim}");
    let scope = plan(
        &db,
        "SELECT record FROM tasks WHERE scope='x' ORDER BY rowid",
    );
    assert!(scope.contains("USING INDEX tasks_scope"), "{scope}");
}

#[tokio::test]
async fn a_version_eleven_database_upgrades_and_keeps_its_tasks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v11.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    fixture(&store).await;
    let before = store.list_tasks(None).await.unwrap();
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "DROP INDEX tasks_status; DROP INDEX tasks_parent; DROP TABLE agents; \
         DROP TABLE agent_config_revisions; DROP TABLE agent_admissions; \
         DROP INDEX agent_inputs_agent; ALTER TABLE agent_inputs DROP COLUMN agent_id;",
    )
    .unwrap();
    downgrade_provider_authority(&db);
    db.pragma_update(None, "user_version", 11).unwrap();
    drop(db);

    let store = SqliteStore::open(&path, 4).unwrap();
    assert_eq!(store.list_tasks(None).await.unwrap(), before);
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(SCHEMA_VERSION, 15);
    let indexes: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('tasks_status','tasks_parent')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(indexes, 2);
    drop(db);
    // Reopening the migrated file passes schema validation again.
    drop(SqliteStore::open(&path, 4).unwrap());
}

/// `interrupt_queued` now filters `retryPending` in SQL through the JSON key,
/// so the record's serialized name must stay exactly that.
#[tokio::test]
async fn retry_pending_serializes_under_the_key_the_sql_filter_reads() {
    let store = SqliteStore::open(":memory:", 4).unwrap();
    store
        .create_chat("s0", None, json!({}), 1000)
        .await
        .unwrap();
    store.submit(task("t", "s0")).await.unwrap();
    let mut record = store.get_task("t").await.unwrap().unwrap();
    record.retry_pending = true;
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["retryPending"], json!(true));
}
