//! Frozen conformance class from `docs/rust/parity.md` ("Reference stores"):
//! every SQLite connection the store creates — file-backed and the in-memory
//! helper alike — must run with `PRAGMA foreign_keys=ON` and fail real
//! foreign-key violations closed.

use super::open;
use rusqlite::{Connection, ffi};
use std::path::Path;

fn pragma(connection: &Connection, name: &str) -> i64 {
    connection
        .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
        .unwrap()
}

fn assert_foreign_key_failure(error: &rusqlite::Error) {
    match error {
        rusqlite::Error::SqliteFailure(failure, message) => {
            assert_eq!(failure.extended_code, ffi::SQLITE_CONSTRAINT_FOREIGNKEY);
            assert_eq!(message.as_deref(), Some("FOREIGN KEY constraint failed"));
        }
        other => panic!("expected a foreign-key constraint failure, got {other:?}"),
    }
}

#[test]
fn file_store_connections_run_with_foreign_keys_enabled_and_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pragma.db");
    let (connection, _lock) = open(&path).unwrap();
    assert_eq!(pragma(&connection, "foreign_keys"), 1);
    // `events.task_id` and `messages.chat_id` both declare REFERENCES.
    assert_foreign_key_failure(
        &connection
            .execute("INSERT INTO events VALUES('ghost',0,'ghost','{}')", [])
            .unwrap_err(),
    );
    assert_foreign_key_failure(
        &connection
            .execute(
                "INSERT INTO messages VALUES('m','ghost-chat',NULL,0,'{}')",
                [],
            )
            .unwrap_err(),
    );
    // Enforcement stays constraint-scoped, not a blanket write refusal.
    connection
        .execute("INSERT INTO chats VALUES('c','{}')", [])
        .unwrap();
    connection
        .execute("INSERT INTO messages VALUES('m','c',NULL,0,'{}')", [])
        .unwrap();
    assert_eq!(pragma(&connection, "foreign_keys"), 1);
    // Flipping the same pragma off re-admits the otherwise-valid row: the
    // rejections above came from the store's connection setup, the only
    // guard standing between this statement and the table.
    connection
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    assert_eq!(pragma(&connection, "foreign_keys"), 0);
    connection
        .execute("INSERT INTO events VALUES('ghost',0,'ghost2','{}')", [])
        .unwrap();
}

#[test]
fn ownership_lock_file_lives_beside_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let (_connection, lock) = open(&path).unwrap();
    let held = lock.unwrap();
    let beside = directory.path().join("owned.db.agentkit-owner");
    let probe = std::fs::OpenOptions::new()
        .write(true)
        .open(&beside)
        .unwrap();
    assert!(probe.try_lock().is_err(), "the beside-DB lock is held");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(&path).unwrap();
        let legacy = std::env::temp_dir()
            .join("agentkit-store-owners")
            .join(format!("{}-{}.lock", metadata.dev(), metadata.ino()));
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(&legacy)
            .unwrap();
        assert!(probe.try_lock().is_err(), "the legacy inode lock is held");
        assert_eq!(held.len(), 2);
    }
    assert!(matches!(open(&path), Err(crate::StoreError::AlreadyOwned)));
}

#[test]
fn in_memory_store_connections_enforce_foreign_keys_too() {
    let (connection, lock) = open(Path::new(":memory:")).unwrap();
    assert!(lock.is_none(), "in-memory stores own no lock file");
    assert_eq!(pragma(&connection, "foreign_keys"), 1);
    assert_foreign_key_failure(
        &connection
            .execute("INSERT INTO attempts VALUES('a1','ghost','{}')", [])
            .unwrap_err(),
    );
}

#[test]
fn provider_authority_migration_preserves_legacy_and_requires_explicit_restoration() {
    use crate::provider_connections::*;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    let legacy_id = provider_connection_identity("legacy");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&super::schema_sql(12)).unwrap();
        db.pragma_update(None, "application_id", super::APPLICATION_ID)
            .unwrap();
        db.pragma_update(None, "user_version", 12).unwrap();
        db.execute(
            "INSERT INTO provider_connections VALUES(?,'digest',1,1000)",
            [&legacy_id],
        )
        .unwrap();
        db.execute("INSERT INTO chats VALUES('chat','{}')", [])
            .unwrap();
        db.execute(
            "INSERT INTO tasks VALUES('settled','chat','completed','{}')",
            [],
        )
        .unwrap();
        let scope = serde_json::json!({"providerId":"p","protocol":"responses","connectionId":legacy_id,"generation":1,"chatId":"chat","branchId":"b","model":"m"});
        db.execute(
            "INSERT INTO run_bindings VALUES('settled','chat',?,0)",
            [scope.to_string()],
        )
        .unwrap();
    }
    drop(crate::SqliteStore::open(&path, 4).unwrap());
    assert!(check_provider_connection(&path, &legacy_id, 1).is_err());
    let new = register_provider_connection_with_continuity(
        &path,
        "legacy",
        Some("new"),
        "url",
        "account",
        1001,
        Some(ProviderContinuityRestore {
            expected_generation: 1,
            previous_generation: 1,
            legacy: true,
        }),
    )
    .unwrap();
    let db = Connection::open(&path).unwrap();
    let scope: String = db
        .query_row(
            "SELECT scope FROM run_bindings WHERE run_id='settled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&scope).unwrap()["connectionId"],
        new.0
    );
    let legacy: String = db
        .query_row(
            "SELECT credential_digest FROM provider_connections WHERE connection_id=?",
            [&legacy_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(legacy, "digest");
    assert!(check_provider_connection(&path, &legacy_id, 1).is_err());
    drop(db);
    drop(crate::SqliteStore::open(&path, 4).unwrap());
    assert_eq!(
        register_scoped_provider_connection(&path, "legacy", Some("new"), "url", "account", 1002)
            .unwrap(),
        new
    );
}
