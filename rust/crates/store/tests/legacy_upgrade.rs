//! B5: native v1/v2 upgrades turn unanswered persisted mutations into
//! CONSERVATIVE uncertain evidence — never a fabricated `tool_result_missing`
//! verdict, never a re-admittable call. The legacy databases below are
//! immutable fixtures: raw SQL frozen from the historical schema, built
//! without the store API so nothing in the shipped code can silently
//! rebase them.

use agentkit_store::{ExecutionReceiptStatus, SqliteStore};
use rusqlite::Connection;

const APPLICATION_ID: i64 = 0x41_4b_52_53;

/// The frozen v2 DDL (v1 differs only in `events.seq`; both predate
/// `tool_executions` entirely).
fn legacy_ddl(v2: bool) -> String {
    let seq = if v2 {
        "seq REAL NOT NULL CHECK(seq>-1)"
    } else {
        "seq INTEGER NOT NULL CHECK(seq>=0)"
    };
    format!(
        "CREATE TABLE tasks(id TEXT PRIMARY KEY, scope TEXT NOT NULL, status TEXT NOT NULL, record TEXT NOT NULL);
CREATE INDEX tasks_scope ON tasks(scope);
CREATE TABLE attempts(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE leases(task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO counters VALUES('fence',0);
CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, {seq}, event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));
CREATE TABLE chats(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE messages(id TEXT PRIMARY KEY, chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, parent_id TEXT REFERENCES messages(id), order_key INTEGER NOT NULL, record TEXT NOT NULL, UNIQUE(chat_id,order_key));
CREATE INDEX messages_chat ON messages(chat_id);
CREATE TABLE outbox(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE proposals(id TEXT PRIMARY KEY, scope TEXT NOT NULL, action_id TEXT, status TEXT NOT NULL, operation_id TEXT UNIQUE, record TEXT NOT NULL);
CREATE UNIQUE INDEX proposal_action ON proposals(scope,action_id) WHERE action_id IS NOT NULL AND status NOT IN ('rejected','invalidated');
CREATE TABLE receipts(operation_id TEXT PRIMARY KEY, proposal_id TEXT NOT NULL UNIQUE REFERENCES proposals(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE settings(namespace TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(namespace,key));
CREATE TABLE run_bindings(run_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE, chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, scope TEXT NOT NULL, state_required INTEGER NOT NULL DEFAULT 0);
CREATE TABLE continuations(run_id TEXT PRIMARY KEY REFERENCES run_bindings(run_id) ON DELETE CASCADE, anchor_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE, record TEXT NOT NULL);"
    )
}

/// Build a legacy database exactly as the pre-receipt releases left it:
/// one answered mutation, one UNANSWERED mutation (the uncertain effect),
/// one assistant record without a run, and one whose run no longer exists.
fn build_legacy_db(path: &std::path::Path, v2: bool) {
    let connection = Connection::open(path).unwrap();
    connection.execute_batch(&legacy_ddl(v2)).unwrap();
    connection
        .pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    connection
        .pragma_update(None, "user_version", if v2 { 2 } else { 1 })
        .unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON;
INSERT INTO tasks VALUES('turn-1','chat','completed','{\"id\":\"turn-1\",\"kind\":\"chat.turn\",\"scope\":\"chat\",\"status\":\"completed\"}');
INSERT INTO tasks VALUES('turn-2','chat','completed','{\"id\":\"turn-2\",\"kind\":\"chat.turn\",\"scope\":\"chat\",\"status\":\"completed\"}');
INSERT INTO chats VALUES('chat','{\"id\":\"chat\"}');
INSERT INTO messages VALUES('m1','chat',NULL,1,'{\"role\":\"assistant\",\"runId\":\"turn-1\",\"toolCalls\":[{\"id\":\"call-a\",\"name\":\"exec\",\"argumentsJson\":\"{\\\"b\\\":1}\"}]}');
INSERT INTO messages VALUES('m2','chat',NULL,2,'{\"role\":\"tool\",\"runId\":\"turn-1\",\"toolCallId\":\"call-a\",\"content\":{\"ok\":true}}');
INSERT INTO messages VALUES('m3','chat',NULL,3,'{\"role\":\"assistant\",\"runId\":\"turn-2\",\"toolCalls\":[{\"id\":\"call-b\",\"name\":\"write\",\"argumentsJson\":\"{\\\"path\\\":\\\"x\\\",\\\"n\\\":1.0}\"}]}');
INSERT INTO messages VALUES('m4','chat',NULL,4,'{\"role\":\"assistant\",\"toolCalls\":[{\"id\":\"call-norun\",\"name\":\"write\",\"argumentsJson\":\"{}\"}]}');
INSERT INTO messages VALUES('m5','chat',NULL,5,'{\"role\":\"assistant\",\"runId\":\"turn-gone\",\"toolCalls\":[{\"id\":\"call-gone\",\"name\":\"write\",\"argumentsJson\":\"{}\"}]}');",
        )
        .unwrap();
    drop(connection);
}

fn canonical(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let sorted = sort(value);
    let text = serde_json::to_string(&sorted).unwrap();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sort(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by_key(|entry| entry.0.clone());
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sort(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort).collect())
        }
        other => other.clone(),
    }
}

async fn assert_conservative_upgrade(path: &std::path::Path) {
    // First open runs the migration.
    let store = SqliteStore::open(path, 8).unwrap();
    let receipts = store.list_executions_by_chat("chat".into()).await.unwrap();
    assert_eq!(
        receipts.len(),
        1,
        "only the unanswered mutation of a still-existing run becomes evidence: {receipts:?}"
    );
    let record = &receipts[0];
    assert_eq!(record.run_id, "turn-2");
    assert_eq!(record.tool_call_id, "call-b");
    assert_eq!(record.tool_name, "write");
    assert_eq!(
        record.status,
        ExecutionReceiptStatus::Admitted,
        "an effect of unknown outcome is ADMITTED-without-result, never a completed verdict"
    );
    assert!(
        record.result.is_none(),
        "no fabricated result for a legacy effect"
    );
    assert_eq!(record.attempt_id, "legacy-upgrade");
    assert_eq!(record.approval_request_id, None);
    // Independent hash oracles (computed outside this crate): canonical
    // `{"n":1,"path":"x"}` — JS-semantics parse (1.0 -> 1) with sorted keys.
    assert_eq!(
        record.args_sha256,
        "8c44d906cbdbb1e949ca9b84ff69ad5f06de9c5294d667e82df22048543a0ff6"
    );
    // The unknown legacy context is the documented hash of `{}`: a constant
    // no future context binding can accidentally match, so uncertainty
    // probes keep matching it.
    assert_eq!(
        record.context_sha256,
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
    // Deterministic identity: the same executionId the admission path would
    // derive — so the receipt IS this call's evidence, not a conflicting one.
    assert_eq!(
        record.execution_id,
        canonical(
            &agentkit_contracts::parse_javascript_json(
                "{\"runId\":\"turn-2\",\"toolCallId\":\"call-b\",\"approvalRequestId\":\"initial\"}"
            )
            .unwrap()
        )
    );
    drop(store);

    // Reopening again must not re-run (or duplicate) the backfill.
    let store = SqliteStore::open(path, 8).unwrap();
    assert_eq!(
        store
            .list_executions_by_chat("chat".into())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_v2_upgrade_backfills_the_unanswered_mutation_as_uncertain() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy-v2.db");
    build_legacy_db(&path, true);
    assert_conservative_upgrade(&path).await;
}

#[tokio::test]
async fn a_v1_upgrade_backfills_the_unanswered_mutation_as_uncertain() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy-v1.db");
    build_legacy_db(&path, false);
    assert_conservative_upgrade(&path).await;
}
