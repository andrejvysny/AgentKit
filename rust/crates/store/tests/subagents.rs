mod support;
use agentkit_store::*;

fn register(child: &str, parent: &str, parent_task: Option<&str>, now: i64) -> RegisterSubagent {
    RegisterSubagent {
        child_chat_id: child.into(),
        parent_chat_id: parent.into(),
        parent_task_id: parent_task.map(str::to_owned),
        role: "scout".into(),
        depth: 1,
        prompt: format!("investigate {child}"),
        model: Some("model-x".into()),
        turn_task_id: None,
        now,
    }
}

fn ids(records: Vec<SubagentRecord>) -> Vec<String> {
    records
        .into_iter()
        .map(|record| record.child_chat_id)
        .collect()
}

#[tokio::test]
async fn register_projects_a_queued_row_and_round_trips_the_wire_shape() {
    let store = SqliteStore::memory(4).unwrap();
    let record = store
        .register_subagent(register("child-1", "parent-1", Some("task-1"), 1000))
        .await
        .unwrap();
    assert_eq!(record.status, "queued");
    assert_eq!(record.created_at, 1000);
    assert_eq!(record.updated_at, 1000);
    assert_eq!(record.result, None);
    assert_eq!(record.error, None);
    assert_eq!(record.turn_task_id, None);
    // The camelCase wire shape the host and napi bindings project.
    let wire = serde_json::to_value(&record).unwrap();
    assert_eq!(wire["childChatId"], "child-1");
    assert_eq!(wire["parentChatId"], "parent-1");
    assert_eq!(wire["parentTaskId"], "task-1");
    assert_eq!(wire["turnTaskId"], serde_json::Value::Null);
    assert_eq!(wire["createdAt"], 1000);
    assert_eq!(store.get_subagent("child-1").await.unwrap(), Some(record));
    assert_eq!(store.get_subagent("ghost").await.unwrap(), None);
}

#[tokio::test]
async fn duplicate_register_and_negative_depth_are_invalid_input() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .register_subagent(register("c", "p", None, 1))
        .await
        .unwrap();
    assert!(matches!(
        store
            .register_subagent(register("c", "other", None, 2))
            .await
            .unwrap_err(),
        StoreError::InvalidInput(_)
    ));
    assert!(matches!(
        store
            .register_subagent(RegisterSubagent {
                depth: -1,
                ..register("d", "p", None, 3)
            })
            .await
            .unwrap_err(),
        StoreError::InvalidInput(_)
    ));
    // Neither rejection left a row behind.
    assert_eq!(store.get_subagent("d").await.unwrap(), None);
    assert_eq!(store.list_subagents("other").await.unwrap().len(), 0);
}

#[tokio::test]
async fn a_terminal_row_is_frozen_and_same_status_retry_is_idempotent() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .register_subagent(register("c", "p", None, 1))
        .await
        .unwrap();
    assert!(
        store
            .update_subagent("c", "running", None, None, 2)
            .await
            .unwrap()
    );
    assert_eq!(
        store.get_subagent("c").await.unwrap().unwrap().status,
        "running"
    );
    assert!(
        store
            .update_subagent("c", "completed", Some("done"), None, 3)
            .await
            .unwrap()
    );
    // Frozen: a DIFFERENT status after terminal is refused, row untouched.
    assert!(
        !store
            .update_subagent("c", "running", Some("rewind"), None, 4)
            .await
            .unwrap()
    );
    let frozen = store.get_subagent("c").await.unwrap().unwrap();
    assert_eq!(frozen.status, "completed");
    assert_eq!(frozen.result.as_deref(), Some("done"));
    assert_eq!(frozen.error, None);
    assert_eq!(frozen.updated_at, 3);
    // Same-status retry is accepted so delivery can retry; values only fill.
    assert!(
        store
            .update_subagent("c", "completed", Some("done"), Some("late"), 5)
            .await
            .unwrap()
    );
    let retried = store.get_subagent("c").await.unwrap().unwrap();
    assert_eq!(retried.result.as_deref(), Some("done"));
    assert_eq!(retried.error.as_deref(), Some("late"));
    assert!(
        store
            .update_subagent("c", "completed", Some("conflicting"), None, 6)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .get_subagent("c")
            .await
            .unwrap()
            .unwrap()
            .result
            .as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn update_refuses_missing_rows_and_unknown_statuses() {
    let store = SqliteStore::memory(4).unwrap();
    assert!(
        !store
            .update_subagent("ghost", "running", None, None, 1)
            .await
            .unwrap()
    );
    store
        .register_subagent(register("c", "p", None, 1))
        .await
        .unwrap();
    // The lifecycle CHECK surfaces as an invalid input, never a panic or a
    // raw database error.
    assert!(matches!(
        store
            .update_subagent("c", "bogus", None, None, 2)
            .await
            .unwrap_err(),
        StoreError::InvalidInput(_)
    ));
    assert_eq!(
        store.get_subagent("c").await.unwrap().unwrap().status,
        "queued"
    );
}

#[tokio::test]
async fn turn_task_tracking_sets_overwrites_and_refuses_missing_rows() {
    let store = SqliteStore::memory(4).unwrap();
    // A spawn that knows its first turn sets it immediately.
    store
        .register_subagent(RegisterSubagent {
            turn_task_id: Some("turn-1".into()),
            ..register("c", "p", None, 1)
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .get_subagent("c")
            .await
            .unwrap()
            .unwrap()
            .turn_task_id
            .as_deref(),
        Some("turn-1")
    );
    // Overwrite is allowed: a follow-up re-points tracking to the new turn.
    assert!(store.set_subagent_turn("c", "turn-2").await.unwrap());
    assert_eq!(
        store
            .get_subagent("c")
            .await
            .unwrap()
            .unwrap()
            .turn_task_id
            .as_deref(),
        Some("turn-2")
    );
    assert!(!store.set_subagent_turn("ghost", "turn-x").await.unwrap());
}

#[tokio::test]
async fn reactivate_only_from_terminal_and_clears_prior_result() {
    // Resume reactivates a terminal row straight to RUNNING. The freeze in
    // `update_subagent` keeps ad-hoc status writes out; this is the ONLY
    // path that may leave a terminal state, it must refuse to resurrect a
    // LIVE row (a race against a still-running turn), and it must wipe the
    // previous delivery so the new turn owns its result/error.
    let store = SqliteStore::memory(64).unwrap();
    store
        .register_subagent(register("c", "p", Some("t"), 10))
        .await
        .unwrap();
    store
        .update_subagent("c", "completed", Some("first answer"), None, 20)
        .await
        .unwrap();

    // A terminal row revives, and the stale result is cleared.
    assert!(store.reactivate_subagent("c", "running", 30).await.unwrap());
    let row = store.get_subagent("c").await.unwrap().unwrap();
    assert_eq!(row.status, "running");
    assert!(
        row.result.is_none() && row.error.is_none(),
        "the reactivated row must not still carry the last turn's delivery"
    );

    // A LIVE row is not reactivated (false, no write): resume already
    // refuses live turns; the store refuses the same race independently.
    assert!(!store.reactivate_subagent("c", "running", 40).await.unwrap());

    // Only queued|running are installable; terminal/unknown are refused.
    let err = store
        .reactivate_subagent("c", "completed", 50)
        .await
        .expect_err("cannot reactivate INTO a terminal status");
    assert!(err.to_string().contains("cannot reactivate"));
    assert!(
        !store
            .reactivate_subagent("missing", "running", 60)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn listings_order_by_created_at_then_insertion_and_count_the_budget() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .register_subagent(register("c1", "p", Some("t1"), 2))
        .await
        .unwrap();
    store
        .register_subagent(register("c2", "p", Some("t1"), 1))
        .await
        .unwrap();
    store
        .register_subagent(register("c3", "p", Some("t2"), 1))
        .await
        .unwrap();
    // created_at ASC, insertion order breaks the tie at timestamp 1.
    assert_eq!(
        ids(store.list_subagents("p").await.unwrap()),
        ["c2", "c3", "c1"]
    );
    assert_eq!(
        ids(store.list_subagents_by_parent_task("t1").await.unwrap()),
        ["c2", "c1"]
    );
    assert!(store.list_subagents("other").await.unwrap().is_empty());
    assert!(
        store
            .list_subagents_by_parent_task("ghost")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.count_active_subagents().await.unwrap(), 3);
    store
        .update_subagent("c1", "completed", Some("r"), None, 3)
        .await
        .unwrap();
    store
        .update_subagent("c2", "failed", None, Some("boom"), 3)
        .await
        .unwrap();
    // Terminals leave the budget; queued stays in.
    assert_eq!(store.count_active_subagents().await.unwrap(), 1);
    store
        .update_subagent("c3", "cancelled", None, None, 3)
        .await
        .unwrap();
    assert_eq!(store.count_active_subagents().await.unwrap(), 0);
    assert!(store.list_active_subagents().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_reaper_scan_enumerates_non_terminal_rows_oldest_first() {
    let store = SqliteStore::memory(4).unwrap();
    store
        .register_subagent(register("a1", "q", None, 5))
        .await
        .unwrap();
    store
        .register_subagent(register("a2", "q", None, 4))
        .await
        .unwrap();
    store
        .register_subagent(register("a3", "q", None, 5))
        .await
        .unwrap();
    store
        .register_subagent(register("t1", "q", None, 1))
        .await
        .unwrap();
    store
        .update_subagent("t1", "completed", Some("r"), None, 2)
        .await
        .unwrap();
    store
        .update_subagent("a2", "running", None, None, 6)
        .await
        .unwrap();
    // Oldest first, insertion order for equal timestamps, terminals (even
    // the oldest row) excluded.
    assert_eq!(
        ids(store.list_active_subagents().await.unwrap()),
        ["a2", "a1", "a3"]
    );
}

#[tokio::test]
async fn registry_rows_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("subagents.db");
    let store = SqliteStore::open(&path, 4).unwrap();
    store
        .register_subagent(register("c", "p", Some("t"), 1))
        .await
        .unwrap();
    store.set_subagent_turn("c", "turn-9").await.unwrap();
    drop(store);
    // Reopening an already-current v10 file works and keeps the row.
    let reopened = SqliteStore::open(&path, 4).unwrap();
    let record = reopened.get_subagent("c").await.unwrap().unwrap();
    assert_eq!(record.prompt, "investigate c");
    assert_eq!(record.turn_task_id.as_deref(), Some("turn-9"));
    assert_eq!(reopened.count_active_subagents().await.unwrap(), 1);
}

#[tokio::test]
async fn a_version_nine_database_upgrades_to_current() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v9.db");
    drop(SqliteStore::open(&path, 4).unwrap());
    // Reduce the fresh file to the exact v9 shape: every table added
    // since (both v10 tables and the v11 review ledger — indexes go with
    // them) disappears and user_version goes back.
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "DROP TABLE subagents; DROP TABLE advisor_findings; DROP TABLE advisor_reviews; DROP INDEX tasks_status; DROP INDEX tasks_parent;",
    )
    .unwrap();
    support::downgrade_provider_authority(&db);
    db.pragma_update(None, "user_version", 9).unwrap();
    drop(db);
    // Reopen migrates 9 -> 10 -> 11 inside open(); the upgrade path is
    // proven by the tables existing again and accepting their rows.
    let store = SqliteStore::open(&path, 4).unwrap();
    store
        .register_subagent(register("c", "p", None, 1))
        .await
        .unwrap();
    assert_eq!(store.count_active_subagents().await.unwrap(), 1);
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name IN \
             ('subagents','advisor_findings','advisor_reviews')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 3);
}
