//! Review round-4 F3: a v18 continuation ledger upgrades to the claim
//! model WITHOUT losing rows — the additive columns (`claimed_attempt_id`,
//! `claimed_task_id`, `claimed_at`, `generation`) and the extended state
//! CHECK (`owned`/`uncertain`) arrive via the version-19 rebuild, and the
//! upgraded ledger is immediately claimable. The "legacy" database below
//! is produced by downgrading a shipped store to the exact frozen v18 DDL
//! (byte-identical to `schema.rs::NATIVE_CONTINUATION_SCHEMA`, which
//! `validate_schema` compares against), so the upgrade path is exercised
//! exactly as an on-disk v18 database would hit it.

use agentkit_store::{
    ClaimInput, CommitContinuation, SettleTask, SqliteStore, SubmitTask, TaskStatus,
};
use rusqlite::Connection;
use serde_json::json;

/// The frozen v18 ledger DDL — verbatim from the shipped schema, with the
/// four claim columns and the extended state CHECK removed.
const V18_REBUILD: &str = r#"
CREATE TABLE native_continuations_scratch(binding_id TEXT, conversation TEXT, parent TEXT, history_position INTEGER, backend TEXT, executable TEXT, config_identity TEXT, system_prompt_revision TEXT, vendor_session_id TEXT, owning_attempt_id TEXT, owning_task_id TEXT, state TEXT, committed_at INTEGER);
INSERT INTO native_continuations_scratch SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at FROM native_continuations;
DROP TABLE native_continuations;
CREATE TABLE native_continuations(binding_id TEXT PRIMARY KEY, conversation TEXT NOT NULL, parent TEXT NOT NULL, history_position INTEGER NOT NULL, backend TEXT NOT NULL, executable TEXT NOT NULL, config_identity TEXT NOT NULL, system_prompt_revision TEXT, vendor_session_id TEXT NOT NULL, owning_attempt_id TEXT NOT NULL, owning_task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, state TEXT NOT NULL CHECK(state IN ('active','superseded','retired')), committed_at INTEGER NOT NULL);
INSERT INTO native_continuations SELECT binding_id, conversation, parent, history_position, backend, executable, config_identity, system_prompt_revision, vendor_session_id, owning_attempt_id, owning_task_id, state, committed_at FROM native_continuations_scratch;
DROP TABLE native_continuations_scratch;
CREATE INDEX native_continuations_scope ON native_continuations(conversation,parent,history_position,backend,executable,config_identity,state);
CREATE INDEX native_continuations_attempt ON native_continuations(owning_attempt_id);
"#;

fn commit(task: &str, attempt: &str, session: &str) -> CommitContinuation {
    CommitContinuation {
        conversation: "chat-1".into(),
        parent: "msg-7".into(),
        history_position: 7,
        backend: "claude-code".into(),
        executable: "/usr/bin/claude".into(),
        config_identity: "cfg-aaa".into(),
        system_prompt_revision: Some("sp-1".into()),
        vendor_session_id: session.into(),
        owning_attempt_id: attempt.into(),
        owning_task_id: task.into(),
        committed_at: 2000,
    }
}

async fn running_task(store: &SqliteStore, task_id: &str, scope_id: &str) -> String {
    store
        .submit(SubmitTask {
            task_id: task_id.into(),
            kind: "native.turn".into(),
            scope_id: scope_id.into(),
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
    store
        .claim(ClaimInput {
            owner_id: "worker-1".into(),
            now: 1500,
            ttl_ms: 600_000,
            kinds: vec!["native.turn".into()],
            busy_scopes: vec![],
            max_attempts: 5,
        })
        .await
        .unwrap()
        .expect("task claimable")
        .lease
        .attempt_id
}

#[tokio::test]
async fn v18_ledger_upgrades_to_the_claim_model_without_losing_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("continuations-v18.db");

    // Publish one ACTIVE binding and one SETTLED superseded row on the
    // current (v19) store first.
    let first_attempt;
    {
        let store = SqliteStore::open(&path, 16).unwrap();
        first_attempt = running_task(&store, "task-c", "chat-turn-c").await;
        store
            .commit_continuation(commit("task-c", &first_attempt, "sess-1"))
            .await
            .unwrap();
        let lease = store.get_lease("task-c").await.unwrap().expect("lease");
        store
            .settle(SettleTask {
                fence: lease.fence(),
                now: 2150,
                status: TaskStatus::Completed,
                error: None,
                events: vec![],
                messages: vec![],
            })
            .await
            .unwrap();
    }

    // Downgrade JUST the ledger table to the frozen v18 shape.
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys=OFF;")
            .unwrap();
        connection.execute_batch(V18_REBUILD).unwrap();
        connection.pragma_update(None, "user_version", 18).unwrap();
    }

    // Reopen: validate_schema must accept the v18 shape and the version-19
    // migration must rebuild it losslessly.
    let store = SqliteStore::open(&path, 16).unwrap();
    let survived = store
        .active_continuation(
            "chat-1",
            "msg-7",
            7,
            "claude-code",
            "/usr/bin/claude",
            "cfg-aaa",
            Some("sp-1"),
        )
        .await
        .unwrap()
        .expect("the active binding survived the rebuild");
    assert_eq!(survived.vendor_session_id, "sess-1");
    assert_eq!(
        survived.generation, 0,
        "legacy rows default to generation 0"
    );
    assert!(survived.claimed_attempt_id.is_none());
    assert!(survived.claimed_task_id.is_none());
    assert!(survived.claimed_at.is_none());

    // The upgraded ledger is immediately claimable under a live lease.
    let attempt_a = running_task(&store, "task-a", "chat-turn-a").await;
    let claimed = store
        .claim_continuation(
            "chat-1",
            "msg-7",
            7,
            "claude-code",
            "/usr/bin/claude",
            "cfg-aaa",
            Some("sp-1"),
            "task-a",
            &attempt_a,
            2200,
        )
        .await
        .unwrap()
        .expect("the migrated ledger is claimable");
    assert_eq!(claimed.state, "owned");
    assert_eq!(claimed.generation, 1);
}
