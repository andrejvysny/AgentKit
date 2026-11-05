use super::*;

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    drop(crate::SqliteStore::open(&path, 64).unwrap());
    (dir, path)
}

fn register(path: &Path, key: &str, secret: &str, endpoint: &str, account: &str) -> (String, u64) {
    register_scoped_provider_connection(path, key, Some(secret), endpoint, account, 1000).unwrap()
}

#[test]
fn independent_stores_do_not_share_connection_identity() {
    let (_a, first) = store();
    let (_b, second) = store();
    assert_ne!(
        register(&first, "same", "key", "url", "account").0,
        register(&second, "same", "key", "url", "account").0
    );
}

#[test]
fn restart_restores_identity_and_rotation_advances_without_rewinding() {
    let (_dir, path) = store();
    let first = register(&path, "main", "key", "url", "account");
    drop(crate::SqliteStore::open(&path, 4).unwrap());
    assert_eq!(first, register(&path, "main", "key", "url", "account"));
    let second = register(&path, "main", "new", "url", "account");
    assert_eq!(first.0, second.0);
    assert_eq!(second.1, 2);
    assert!(check_provider_connection(&path, &first.0, first.1).is_err());
    assert_eq!(register(&path, "main", "key", "url", "account").1, 3);
}

#[test]
fn endpoint_and_account_changes_retire_old_identity() {
    let (_dir, path) = store();
    let a = register(&path, "main", "key", "one", "account");
    let b = register(&path, "main", "key", "two", "account");
    let c = register(&path, "main", "key", "two", "other");
    assert_ne!(a.0, b.0);
    assert_ne!(b.0, c.0);
    assert!(check_provider_connection(&path, &a.0, a.1).is_err());
    assert!(check_provider_connection(&path, &b.0, b.1).is_err());
    assert!(check_provider_connection(&path, &c.0, c.1).is_ok());
    assert_eq!(register(&path, "main", "key", "one", "account").1, 2);
}

#[test]
fn revoke_reregister_never_revives_old_authority() {
    let (_dir, path) = store();
    let first = register(&path, "main", "key", "url", "account");
    assert!(revoke_persisted_provider_connection(&path, &first.0).unwrap());
    assert!(!revoke_persisted_provider_connection(&path, &first.0).unwrap());
    assert!(check_provider_connection(&path, &first.0, first.1).is_err());
    let second = register(&path, "main", "key", "url", "account");
    assert_eq!(second.1, 2);
    assert!(check_provider_connection(&path, &first.0, first.1).is_err());
}

#[test]
fn concurrent_registration_has_one_identity_and_generation() {
    let (_dir, path) = store();
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || register(&path, "same", "key", "url", "account"))
        })
        .collect();
    let values: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert!(values.iter().all(|value| value == &values[0]));
    assert_eq!(values[0].1, 1);
}

fn seed_scopes(path: &Path, id: &str) {
    let db = Connection::open(path).unwrap();
    db.execute("INSERT INTO chats VALUES('chat','{}')", [])
        .unwrap();
    db.execute(
        "INSERT INTO messages VALUES('anchor','chat',NULL,1,'{}')",
        [],
    )
    .unwrap();
    for (run, status) in [("settled", "completed"), ("active", "running")] {
        db.execute(
            "INSERT INTO tasks VALUES(?,'chat',?,'{}')",
            params![run, status],
        )
        .unwrap();
        let scope = json!({"providerId":"p","protocol":"responses","connectionId":id,
            "generation":1,"chatId":"chat","branchId":"anchor","model":"m"});
        db.execute(
            "INSERT INTO run_bindings VALUES(?,'chat',?,1)",
            params![run, json(&db, &scope).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO continuations VALUES(?,'anchor',?)",
            params![
                run,
                json(
                    &db,
                    &json!({"runId":run,"anchorMessageId":"anchor","state":{"scope":scope}})
                )
                .unwrap()
            ],
        )
        .unwrap();
    }
}

fn generation(path: &Path, run: &str) -> u64 {
    let db = Connection::open(path).unwrap();
    let scope: String = db
        .query_row(
            "SELECT scope FROM run_bindings WHERE run_id=?",
            [run],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str::<Value>(&scope).unwrap()["generation"]
        .as_u64()
        .unwrap()
}

#[test]
fn rotation_preserves_history_and_only_explicit_continuity_moves_settled_work() {
    let (_dir, path) = store();
    let first = register(&path, "main", "key", "url", "account");
    seed_scopes(&path, &first.0);
    let next = register(&path, "main", "new", "url", "account");
    assert_eq!(generation(&path, "settled"), 1);
    assert_eq!(generation(&path, "active"), 1);
    assert!(restore_provider_continuity(&path, &next.0, 1, 1, "url", "account").is_err());
    assert!(restore_provider_continuity(&path, &next.0, 2, 1, "wrong", "account").is_err());
    assert!(restore_provider_continuity(&path, &next.0, 2, 1, "url", "").is_err());
    assert_eq!(
        restore_provider_continuity(&path, &next.0, 2, 1, "url", "account").unwrap(),
        1
    );
    assert_eq!(generation(&path, "settled"), 2);
    assert_eq!(generation(&path, "active"), 1);
    let db = Connection::open(&path).unwrap();
    let record: String = db
        .query_row(
            "SELECT record FROM continuations WHERE run_id='active'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&record).unwrap()["state"]["scope"]["generation"],
        1
    );
}

#[test]
fn invalid_and_foreign_registration_never_create_or_reset_history() {
    let (dir, path) = store();
    let missing = dir.path().join("missing.sqlite");
    assert!(register_provider_connection(&missing, "k", None, 0).is_err());
    assert!(!missing.exists());
    assert!(register_provider_connection(Path::new(":memory:"), "k", None, 0).is_err());
    assert!(register_provider_connection(&path, "", None, 0).is_err());
    let foreign = dir.path().join("foreign.sqlite");
    Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE keep(x);INSERT INTO keep VALUES(7)")
        .unwrap();
    assert!(matches!(
        register_provider_connection(&foreign, "k", None, 0),
        Err(StoreError::ForeignDatabase)
    ));
    let count: i64 = Connection::open(foreign)
        .unwrap()
        .query_row("SELECT x FROM keep", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 7);
}

#[test]
fn failed_continuity_compare_and_set_rolls_back_credential_rotation() {
    let (_dir, path) = store();
    let first = register(&path, "main", "key", "url", "account");
    let restore = ProviderContinuityRestore {
        expected_generation: 3,
        previous_generation: 1,
        legacy: false,
    };
    assert!(
        register_provider_connection_with_continuity(
            &path,
            "main",
            Some("new"),
            "url",
            "account",
            1001,
            Some(restore)
        )
        .is_err()
    );
    assert!(check_provider_connection(&path, &first.0, first.1).is_ok());
    assert_eq!(register(&path, "main", "key", "url", "account"), first);
}
