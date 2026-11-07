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

#[cfg(unix)]
#[test]
fn orphaned_legacy_owner_locks_are_swept_live_and_held_locks_are_not() {
    // AGENTKIT-74 cleanup sweep: the temp-dir owner-lock directory must
    // not accumulate one file per opened database forever — but a sweep
    // may only remove what it can PROVE is orphaned.
    use std::os::unix::fs::MetadataExt;
    let directory = tempfile::tempdir().unwrap();
    let owners = std::env::temp_dir().join("agentkit-store-owners");
    std::fs::create_dir_all(&owners).unwrap();
    // Opening a store writes the database path into the legacy lock.
    let live_db = directory.path().join("live.db");
    let (_connection, _locks) = open(&live_db).unwrap();
    let live_meta = std::fs::metadata(&live_db).unwrap();
    let live_lock = owners.join(format!("{}-{}.lock", live_meta.dev(), live_meta.ino()));
    assert_eq!(
        std::fs::read_to_string(&live_lock).unwrap().trim(),
        live_db.canonicalize().unwrap().display().to_string(),
        "the lock records the database it keys"
    );
    // (1) Orphan: the name claims a database that no longer exists.
    let gone_db = directory.path().join("gone.db");
    std::fs::write(&gone_db, "").unwrap();
    let gone_meta = std::fs::metadata(&gone_db).unwrap();
    std::fs::remove_file(&gone_db).unwrap();
    let gone_lock = owners.join(format!("{}-{}.lock", gone_meta.dev(), gone_meta.ino()));
    std::fs::write(&gone_lock, "/definitely/removed/agentkit-gone.db\n").unwrap();
    // (2) Replaced: the recorded path exists but at a different inode.
    let moved_db = directory.path().join("moved.db");
    std::fs::write(&moved_db, "").unwrap();
    let old_meta = std::fs::metadata(&moved_db).unwrap();
    std::fs::remove_file(&moved_db).unwrap();
    std::fs::write(&moved_db, "").unwrap();
    let moved_lock = owners.join(format!("{}-{}.lock", old_meta.dev(), old_meta.ino()));
    std::fs::write(&moved_lock, format!("{}\n", moved_db.display())).unwrap();
    // (3) Untagged legacy file (pre-sweep builds): never provably orphaned.
    let legacy_lock = owners.join("1-1.lock");
    std::fs::write(&legacy_lock, "").unwrap();
    // (4) Orphan content but the flock is held: somebody is using it.
    let held_db = directory.path().join("held.db");
    std::fs::write(&held_db, "").unwrap();
    let held_meta = std::fs::metadata(&held_db).unwrap();
    let held_lock = owners.join(format!("{}-{}.lock", held_meta.dev(), held_meta.ino()));
    std::fs::write(&held_lock, "/definitely/removed/agentkit-held.db\n").unwrap();
    let hold = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&held_lock)
        .unwrap();
    assert!(hold.try_lock().is_ok(), "flock the orphan");
    super::sweep_owner_locks(&owners, std::time::Duration::ZERO);
    assert!(
        !gone_lock.exists() && !moved_lock.exists(),
        "provably orphaned locks are swept"
    );
    for keep in [&live_lock, &legacy_lock, &held_lock] {
        assert!(keep.exists(), "{keep:?} is not provably orphaned");
    }
    drop(hold);
    // Released, the orphan goes on the next sweep.
    super::sweep_owner_locks(&owners, std::time::Duration::ZERO);
    assert!(!held_lock.exists());
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
