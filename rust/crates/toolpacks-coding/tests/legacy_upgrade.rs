//! B5 (native legacy upgrade): a v1/v2 database upgraded to the current
//! schema must NOT surface old unanswered mutations as plain
//! `tool_result_missing` verdicts. The upgrade seeds each persisted,
//! never-answered assistant tool call with an `admitted`-status receipt —
//! the store's honest UNCERTAIN evidence (ADR 0020): history reconciliation
//! renders admitted-without-result as `tool_effect_uncertain` (never a
//! synthetic missing-result success/failure), and re-admission of the exact
//! operation collides with the deterministic receipt id, so the effect is
//! never automatically replayed. This covers the NATIVE schema upgrade
//! only; the separate TS schema-9 migration is untouched.

use agentkit_store::*;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

fn call(id: &str, name: &str, arguments: &str) -> Value {
    json!({"id": id, "name": name, "argumentsJson": arguments})
}

fn message(chat: &str, id: &str, role: &str, run: Option<&str>) -> MessageInput {
    MessageInput {
        id: Some(id.into()),
        chat_id: chat.into(),
        run_id: run.map(str::to_owned),
        role: role.into(),
        content: json!(id),
        ..Default::default()
    }
}

/// Seed a chat whose persisted history contains:
///  - run-a: call-1 (coding.run_command, UNANSWERED mutation) and
///    call-2 (coding.read_file, answered by a tool record),
///  - run-b: call-3 (coding.run_tests, UNANSWERED),
///  - a run-less assistant declaring call-4 (no run can own evidence),
///  - a dangling tool record answering call-999 (noise).
async fn seed(path: &Path) {
    let store = SqliteStore::open(path, 4).unwrap();
    store
        .create_chat("chat", None, json!({}), 1000)
        .await
        .unwrap();
    store
        .submit_turn(
            SubmitTask {
                task_id: "run-a".into(),
                kind: "chat.turn".into(),
                scope_id: "chat".into(),
                payload: json!({"chatId": "chat"}),
                priority: 0,
                now: 1000,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            },
            vec![
                message("chat", "u1", "user", Some("run-a")),
                MessageInput {
                    role: "assistant".into(),
                    tool_calls: Some(json!([
                        call(
                            "call-1",
                            "coding.run_command",
                            "{\"command\":\"make install\"}"
                        ),
                        call("call-2", "coding.read_file", "{\"path\":\"a.txt\"}"),
                    ])),
                    ..message("chat", "a1", "assistant", Some("run-a"))
                },
                MessageInput {
                    tool_call_id: Some("call-2".into()),
                    content: json!({"ok": true, "text": "file"}),
                    ..message("chat", "t2", "tool", Some("run-a"))
                },
            ],
        )
        .await
        .unwrap();
    store
        .submit_turn(
            SubmitTask {
                task_id: "run-b".into(),
                kind: "chat.turn".into(),
                scope_id: "chat".into(),
                payload: json!({"chatId": "chat"}),
                priority: 0,
                now: 1001,
                available_at: None,
                depends_on: vec![],
                parent_task_id: None,
                exclusive_scope: false,
            },
            vec![
                message("chat", "u2", "user", Some("run-b")),
                MessageInput {
                    role: "assistant".into(),
                    tool_calls: Some(json!([call("call-3", "coding.run_tests", "{}")])),
                    ..message("chat", "a2", "assistant", Some("run-b"))
                },
                MessageInput {
                    tool_call_id: Some("call-999".into()),
                    content: json!({"ok": true}),
                    ..message("chat", "t9", "tool", Some("run-b"))
                },
            ],
        )
        .await
        .unwrap();
    // An assistant record without a run: it can never own durable evidence.
    store
        .append_message(
            MessageInput {
                role: "assistant".into(),
                tool_calls: Some(json!([call("call-4", "coding.write_file", "{}")])),
                ..message("chat", "a3", "assistant", None)
            },
            None,
            1002,
        )
        .await
        .unwrap();
    drop(store);
}

/// Drop the database down to a genuine v1 or v2 shape: the receipts and
/// session-input tables disappear and `user_version` goes back.
fn downgrade(path: &Path, version: i64) {
    assert!(version == 1 || version == 2);
    let db = Connection::open(path).unwrap();
    db.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
    if version == 1 {
        db.execute_batch(
            "BEGIN;
             ALTER TABLE events RENAME TO events_v2;
             CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, seq INTEGER NOT NULL CHECK(seq>=0), event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));
             INSERT INTO events SELECT task_id,seq,event_id,record FROM events_v2;
             DROP TABLE events_v2;
             COMMIT;",
        )
        .unwrap();
    }
    db.execute_batch(
        "BEGIN;
         DROP TABLE IF EXISTS tool_executions;
         DROP TABLE IF EXISTS agent_inputs;
         DELETE FROM counters WHERE name='input_order';
         COMMIT;",
    )
    .unwrap();
    db.pragma_update(None, "user_version", version).unwrap();
    let check: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, version);
    let tables: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='tool_executions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0, "the legacy database must have NO receipt table");
    drop(db);
}

/// sha-256 hex over the exact canonical string the admission path hashes.
fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The precomputed canonical identities for the seeded calls. The canonical
/// form is JSON with sorted keys (`serde_json::to_string`), identical to the
/// host's `args_sha256` / `execution_id` derivation.
fn expected_execution_id(run: &str, tool_call: &str) -> String {
    sha256(&format!(
        "{{\"approvalRequestId\":\"initial\",\"runId\":\"{run}\",\"toolCallId\":\"{tool_call}\"}}"
    ))
}

fn assert_uncertain_evidence(record: &ExecutionReceiptRecord, run: &str, call_id: &str) {
    assert_eq!(record.run_id, run);
    assert_eq!(record.tool_call_id, call_id);
    assert_eq!(record.chat_id, "chat");
    assert_eq!(
        record.status,
        ExecutionReceiptStatus::Admitted,
        "a legacy unanswered mutation must be UPGRADED INTO admitted (uncertain) evidence"
    );
    assert!(
        record.result.is_none(),
        "the upgrade must never fabricate a result (no success, no failure)"
    );
    assert_eq!(record.execution_id, expected_execution_id(run, call_id));
    assert_eq!(record.attempt_id, "legacy-upgrade");
    assert!(record.approval_request_id.is_none());
}

async fn exercise_upgrade(version: i64) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(format!("legacy-v{version}.db"));
    seed(&path).await;
    downgrade(&path, version);

    // --- The upgrade itself: reopen admits the database at the current
    // version and seeds conservative uncertain evidence.
    let store = SqliteStore::open(&path, 4).unwrap();
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    drop(db);

    let receipts = store
        .list_executions_by_chat("chat".into())
        .await
        .expect("receipt reads never fail as absence");
    let mut keyed: Vec<(String, String)> = receipts
        .iter()
        .map(|record| (record.run_id.clone(), record.tool_call_id.clone()))
        .collect();
    keyed.sort();
    assert_eq!(
        keyed,
        vec![
            ("run-a".to_owned(), "call-1".to_owned()),
            ("run-b".to_owned(), "call-3".to_owned()),
        ],
        "exactly the UNANSWERED calls of owned runs become uncertain evidence; \
         answered calls and run-less declarations stay untouched"
    );
    for record in &receipts {
        if record.run_id == "run-a" {
            assert_eq!(record.tool_name, "coding.run_command");
            // canonical `{"command":"make install"}` (already key-sorted)
            assert_eq!(record.args_sha256, sha256("{\"command\":\"make install\"}"));
            assert_uncertain_evidence(record, "run-a", "call-1");
        } else {
            assert_eq!(record.tool_name, "coding.run_tests");
            assert_eq!(record.args_sha256, sha256("{}"));
            assert_uncertain_evidence(record, "run-b", "call-3");
        }
        // The legacy context binding is unknowable; the documented
        // empty-object canonical hash keeps the record valid while matching
        // no future admission context.
        assert_eq!(
            record.context_sha256,
            sha256("{}"),
            "legacy context hash is the canonical hash of the empty object"
        );
    }

    // --- Never auto-replayed: re-admitting the exact operation (deterministic
    // receipt identity per (run, call, initial)) is refused — the caller
    // must not invoke the body — and the evidence stays uncertain.
    let claimed = store
        .claim(ClaimInput {
            owner_id: "worker".into(),
            now: 2000,
            ttl_ms: 60_000,
            kinds: vec![],
            busy_scopes: vec![],
            max_attempts: 3,
        })
        .await
        .expect("claim")
        .expect("a legacy run is claimable");
    let admitted = store
        .admit_execution(
            claimed.lease.fence(),
            ExecutionReceiptAdmission {
                execution_id: expected_execution_id("run-a", "call-1"),
                run_id: "run-a".into(),
                chat_id: "chat".into(),
                tool_call_id: "call-1".into(),
                tool_name: "coding.run_command".into(),
                args_sha256: sha256("{\"command\":\"make install\"}"),
                context_sha256: sha256("{\"real\":\"context\"}"),
                attempt_id: "attempt-2".into(),
                approval_request_id: None,
                approval_generation: None,
                now: 2001,
            },
        )
        .await
        .expect("admission probe");
    assert!(
        !admitted,
        "the old unanswered mutation must never be automatically replayed: \
         admission of the same (run, tool call, approval) collides with the \
         conservative receipt and returns false"
    );
    let after = store.list_executions("run-a".into()).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].status, ExecutionReceiptStatus::Admitted);
    assert!(after[0].result.is_none());
    drop(store);

    // --- Idempotence across further reopens: no duplicates, no drift.
    let store = SqliteStore::open(&path, 4).unwrap();
    let again = store.list_executions_by_chat("chat".into()).await.unwrap();
    assert_eq!(again.len(), 2);
    let db = Connection::open(&path).unwrap();
    let rows: i64 = db
        .query_row("SELECT count(*) FROM tool_executions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn native_v1_upgrade_surfaces_old_unanswered_mutations_as_uncertain() {
    exercise_upgrade(1).await;
}

#[tokio::test]
async fn native_v2_upgrade_surfaces_old_unanswered_mutations_as_uncertain() {
    exercise_upgrade(2).await;
}
