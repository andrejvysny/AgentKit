use agentkit_store::*;
use serde_json::{Value, json};

fn write(namespace: &str, key: &str, value: Option<Value>) -> SettingWrite {
    SettingWrite {
        namespace: namespace.into(),
        key: key.into(),
        value,
    }
}
fn changes(revision: u32) -> Vec<SettingWrite> {
    vec![
        write("http", "connections", Some(json!({"revision":revision}))),
        write("models", "provider", Some(json!({"revision":revision}))),
        write("catalog", "provider", Some(json!({"revision":revision}))),
    ]
}
async fn seed(store: &SqliteStore) {
    for change in changes(1) {
        store
            .set_setting(&change.namespace, &change.key, change.value.unwrap())
            .await
            .unwrap();
    }
}
async fn snapshot(store: &SqliteStore) -> Vec<Option<Value>> {
    let mut values = vec![];
    for (namespace, key) in [
        ("http", "connections"),
        ("models", "provider"),
        ("catalog", "provider"),
    ] {
        values.push(store.get_setting(namespace, key).await.unwrap());
    }
    values
}

#[tokio::test]
async fn concurrent_guarded_multisets_have_one_winner_without_partial_namespace_updates() {
    let directory = tempfile::tempdir().unwrap();
    for store in [
        SqliteStore::memory(8).unwrap(),
        SqliteStore::open(directory.path().join("cas.db"), 8).unwrap(),
    ] {
        seed(&store).await;
        let other = store.clone();
        let expected = Some(json!({"revision":1}));
        let (first, second) = tokio::join!(
            store.compare_and_set_settings("http", "connections", expected.clone(), changes(2)),
            other.compare_and_set_settings("http", "connections", expected, changes(3))
        );
        let revision = match (first.unwrap(), second.unwrap()) {
            (SettingsCommit::Applied, SettingsCommit::Conflict) => 2,
            (SettingsCommit::Conflict, SettingsCommit::Applied) => 3,
            result => panic!("unexpected CAS outcomes: {result:?}"),
        };
        assert_eq!(
            snapshot(&store).await,
            vec![Some(json!({"revision":revision})); 3]
        );
        assert!(matches!(
            store
                .compare_and_set_settings("http", "connections", None, changes(4))
                .await
                .unwrap(),
            SettingsCommit::Conflict
        ));
        assert_eq!(
            snapshot(&store).await,
            vec![Some(json!({"revision":revision})); 3]
        );
    }
}

#[tokio::test]
async fn known_trigger_abort_explicitly_rolls_back_snapshot_and_all_related_writes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("abort.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    seed(&store).await;
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_catalog BEFORE UPDATE ON settings WHEN NEW.namespace='catalog' BEGIN SELECT RAISE(ABORT,'catalog refused'); END;").unwrap();
    let outcome = store
        .compare_and_set_settings(
            "http",
            "connections",
            Some(json!({"revision":1})),
            changes(2),
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome,SettingsCommit::Aborted(StoreError::Database(rusqlite::Error::SqliteFailure(_,Some(message)))) if message=="catalog refused")
    );
    assert_eq!(snapshot(&store).await, vec![Some(json!({"revision":1})); 3]);
    db.execute_batch("DROP TRIGGER reject_catalog;").unwrap();
    drop(db);
    drop(store);
    let reopened = SqliteStore::open(&path, 8).unwrap();
    assert_eq!(
        snapshot(&reopened).await,
        vec![Some(json!({"revision":1})); 3]
    );
}

#[tokio::test]
async fn rollback_failure_is_outer_uncertainty_even_when_sqlite_already_rolled_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollback.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    seed(&store).await;
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER rollback_catalog BEFORE UPDATE ON settings WHEN NEW.namespace='catalog' BEGIN SELECT RAISE(ROLLBACK,'transaction rolled back'); END;").unwrap();
    let outcome = store
        .compare_and_set_settings(
            "http",
            "connections",
            Some(json!({"revision":1})),
            changes(2),
        )
        .await;
    assert!(
        matches!(outcome,Err(StoreError::Database(error)) if error.to_string().contains("cannot rollback"))
    );
    assert_eq!(snapshot(&store).await, vec![Some(json!({"revision":1})); 3]);
}

#[tokio::test]
async fn commit_failure_is_outer_uncertainty_instead_of_a_known_abort() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("commit.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    seed(&store).await;
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE parent_for_test(id INTEGER PRIMARY KEY);
        CREATE TABLE deferred_for_test(id INTEGER PRIMARY KEY,parent INTEGER REFERENCES parent_for_test(id) DEFERRABLE INITIALLY DEFERRED);
        CREATE TRIGGER defer_catalog AFTER UPDATE ON settings WHEN NEW.namespace='catalog' BEGIN INSERT INTO deferred_for_test VALUES(1,99); END;").unwrap();
    let outcome = store
        .compare_and_set_settings(
            "http",
            "connections",
            Some(json!({"revision":1})),
            changes(2),
        )
        .await;
    assert!(
        matches!(outcome,Err(StoreError::Database(rusqlite::Error::SqliteFailure(error,_))) if error.code==rusqlite::ErrorCode::ConstraintViolation)
    );
    assert_eq!(snapshot(&store).await, vec![Some(json!({"revision":1})); 3]);
}

#[tokio::test]
async fn guard_read_error_is_known_aborted_only_after_successful_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("read.db");
    let store = SqliteStore::open(&path, 8).unwrap();
    seed(&store).await;
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "UPDATE settings SET value='not json' WHERE namespace='http' AND key='connections'",
        [],
    )
    .unwrap();
    assert!(matches!(
        store
            .compare_and_set_settings("http", "connections", None, changes(2))
            .await
            .unwrap(),
        SettingsCommit::Aborted(StoreError::Json(_))
    ));
    assert_eq!(
        store.get_setting("models", "provider").await.unwrap(),
        Some(json!({"revision":1}))
    );
    assert_eq!(
        store.get_setting("catalog", "provider").await.unwrap(),
        Some(json!({"revision":1}))
    );
}

#[tokio::test]
async fn typed_json_guard_equality_distinguishes_booleans_strings_and_missing_values() {
    let directory = tempfile::tempdir().unwrap();
    for store in [
        SqliteStore::memory(8).unwrap(),
        SqliteStore::open(directory.path().join("json.db"), 8).unwrap(),
    ] {
        store
            .set_setting("guard", "value", json!(true))
            .await
            .unwrap();
        for expected in [json!(1), json!("true"), Value::Null] {
            assert!(matches!(
                store
                    .compare_and_set_settings(
                        "guard",
                        "value",
                        Some(expected),
                        vec![write("guard", "value", Some(json!(false)))]
                    )
                    .await
                    .unwrap(),
                SettingsCommit::Conflict
            ));
        }
        assert!(matches!(
            store
                .compare_and_set_settings(
                    "guard",
                    "value",
                    Some(json!(true)),
                    vec![write("guard", "value", Some(json!(1)))]
                )
                .await
                .unwrap(),
            SettingsCommit::Applied
        ));
        let marker = agentkit_contracts::parse_javascript_json(
            r#"{"$serde_json::private::Number":"true","enabled":true}"#,
        )
        .unwrap();
        assert!(matches!(
            store
                .compare_and_set_settings(
                    "guard",
                    "value",
                    Some(json!(1.0)),
                    vec![write("guard", "value", Some(marker.clone()))]
                )
                .await
                .unwrap(),
            SettingsCommit::Applied
        ));
        assert!(matches!(
            store
                .compare_and_set_settings(
                    "guard",
                    "value",
                    Some(marker),
                    vec![
                        write("guard", "value", None),
                        write("other", "null", Some(Value::Null))
                    ]
                )
                .await
                .unwrap(),
            SettingsCommit::Applied
        ));
        assert_eq!(store.get_setting("guard", "value").await.unwrap(), None);
        assert_eq!(
            store.get_setting("other", "null").await.unwrap(),
            Some(Value::Null)
        );
    }
}
