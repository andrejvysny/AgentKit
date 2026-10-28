use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

const APPLICATION_ID: i64 = 0x414b5253;
/// The schema version this build creates and accepts; migrations land as
/// `version < N` steps below.
pub const SCHEMA_VERSION: i64 = 4;
const VERSION: i64 = SCHEMA_VERSION;

fn ownership(path: &Path) -> Result<Option<File>> {
    if path == Path::new(":memory:") {
        return Ok(None);
    }
    let canonical = if path.exists() {
        path.canonicalize()?
    } else {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        parent.canonicalize()?.join(
            path.file_name()
                .ok_or_else(|| StoreError::InvalidInput("database path has no file name".into()))?,
        )
    };
    let lock_path = ownership_path(&canonical)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock().map_err(|_| StoreError::AlreadyOwned)?;
    Ok(Some(lock))
}

#[cfg(unix)]
fn ownership_path(path: &Path) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    // SQLite's platform VFS may share the database's OS lock primitive.
    // A separate inode-keyed lock covers hard-link aliases without interfering.
    let database = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let metadata = database.metadata()?;
    let directory = std::env::temp_dir().join("agentkit-store-owners");
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join(format!("{}-{}.lock", metadata.dev(), metadata.ino())))
}

#[cfg(not(unix))]
fn ownership_path(path: &Path) -> Result<std::path::PathBuf> {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".agentkit-owner");
    Ok(lock.into())
}

pub(crate) fn open(path: &Path) -> Result<(Connection, Option<File>)> {
    let lock = ownership(path)?;
    let mut connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let identity: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    let fresh = identity == 0 && version == 0 && count == 0;
    if !fresh && identity != APPLICATION_ID {
        return Err(StoreError::ForeignDatabase);
    }
    if !fresh && ![1, 2, 3, VERSION].contains(&version) {
        return Err(StoreError::UnsupportedVersion(version));
    }
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    if fresh {
        let transaction = connection.transaction()?;
        transaction.execute_batch(&schema_sql(VERSION))?;
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", VERSION)?;
        transaction.commit()?;
    } else {
        validate_schema(&connection, version)?;
        if version == 1 {
            migrate_events(&mut connection)?;
        }
        if version < 3 {
            migrate_receipts(&mut connection)?;
        }
        if version < 4 {
            migrate_sessions(&mut connection)?;
        }
    }
    if path != Path::new(":memory:") {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok((connection, lock))
}

fn validate_schema(connection: &Connection, version: i64) -> Result<()> {
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(&schema_sql(version))?;
    if schema_signature(connection)? != schema_signature(&expected)? {
        return Err(StoreError::ForeignDatabase);
    }
    let counter_ok:bool=connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM counters WHERE name='fence' AND typeof(value)='integer' AND value>=0)",
        [],|row|row.get(0),
    )?;
    if !counter_ok {
        return Err(StoreError::ForeignDatabase);
    }
    let mut foreign_keys = connection.prepare("PRAGMA foreign_key_check")?;
    if foreign_keys.query([])?.next()?.is_some() {
        return Err(StoreError::ForeignDatabase);
    }
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(StoreError::InvalidInput(format!(
            "database integrity: {integrity}"
        )));
    }
    Ok(())
}

fn schema_sql(version: i64) -> String {
    let mut sql = if version == 1 {
        SCHEMA.to_owned()
    } else {
        SCHEMA.replace(
            "seq INTEGER NOT NULL CHECK(seq>=0)",
            "seq REAL NOT NULL CHECK(seq>-1)",
        )
    };
    if version >= 3 {
        sql.push_str(RECEIPTS_SCHEMA);
    }
    if version >= 4 {
        sql.push_str(SESSIONS_SCHEMA);
    }
    sql
}

/// Durable execution receipts (ADR 0020), additive at schema version 3.
/// `execution_id` is the deterministic sha-256 of
/// `{runId, toolCallId, approvalRequestId ?? "initial"}`; the UNIQUE
/// `approval_request_id` column makes each answered approval single-use.
const RECEIPTS_SCHEMA: &str = r#"
CREATE TABLE tool_executions(execution_id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, approval_request_id TEXT UNIQUE, record TEXT NOT NULL);
CREATE INDEX tool_executions_run ON tool_executions(run_id);
"#;

fn migrate_events(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch("ALTER TABLE events RENAME TO events_v1;")?;
    transaction.execute_batch("CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, seq REAL NOT NULL CHECK(seq>-1), event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));")?;
    transaction.execute_batch("INSERT INTO events SELECT task_id,seq,event_id,record FROM events_v1; DROP TABLE events_v1;")?;
    transaction.pragma_update(None, "user_version", 2)?;
    transaction.commit()?;
    Ok(())
}

fn migrate_receipts(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(RECEIPTS_SCHEMA)?;
    backfill_legacy_uncertain(&transaction)?;
    transaction.pragma_update(None, "user_version", 3)?;
    transaction.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// B5: legacy unanswered mutations become CONSERVATIVE uncertain evidence.
//
// A v1/v2 database predates `tool_executions` entirely. With an EMPTY
// receipt table, ordinary history assembly treats every persisted assistant
// tool call whose result never landed as plain `tool_result_missing` — a
// fabricated verdict about an effect nobody can prove, and a resumed run
// could even re-admit the mutation. Instead the upgrade records each such
// call as an `admitted`-status receipt: admitted-without-result is the
// store's honest representation of an UNCERTAIN effect (ADR 0020). The host
// reconciles it as `tool_effect_uncertain` (never a synthetic
// missing-result success/failure), the durable-execution guard refuses to
// re-admit the same (run, tool call) — no automatic replay — and explicit
// recovery is the only path that writes a real result over it.
//
// Identity fields are computed with the SAME canonicalization the admission
// path uses (JS-semantics parse of the persisted `argumentsJson`, object
// keys sorted, `serde_json::to_string`, sha-256 hex), so the backfilled
// record is recognized as this call's evidence instead of a conflicting one.
// `contextSha256` of a legacy effect is genuinely unknown: it is the
// canonical hash of the empty object, a documented constant that matches no
// future context binding (uncertainty probes match on tool name + argument
// hash, so the unknown context still blocks reissue).

/// The documented legacy context hash: canonical sha-256 of `{}`.
const LEGACY_CONTEXT_SHA256: &str =
    "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

/// Canonical JSON: object keys sorted (JavaScript-semantic number
/// spellings preserved from `parse_javascript_json`), exactly the shape the
// host's `args_sha256`/`execution_id` hashing feeds to sha-256.
fn canonical_sha256(value: &serde_json::Value) -> String {
    let sorted = sort_keys(value);
    let text = serde_json::to_string(&sorted).unwrap_or_else(|_| "null".into());
    sha256_hex(&text)
}

fn sort_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by_key(|entry| entry.0.clone());
            let sorted: serde_json::Map<String, serde_json::Value> = entries
                .into_iter()
                .map(|(key, value)| (key.clone(), sort_keys(value)))
                .collect();
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_keys).collect())
        }
        other => other.clone(),
    }
}

fn backfill_legacy_uncertain(transaction: &rusqlite::Transaction<'_>) -> Result<()> {
    use std::collections::HashSet;
    // (chat, run, tool-call-id) pairs already answered by a persisted tool
    // record of the same run — the exact key history reconciliation uses.
    let mut answered: HashSet<(String, String, String)> = HashSet::new();
    let mut declared: Vec<(String, String, String, String, String)> = Vec::new();
    {
        let mut statement =
            transaction.prepare("SELECT chat_id, record FROM messages ORDER BY rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (chat_id, record_text) = row?;
            let Ok(record) = agentkit_contracts::parse_javascript_json(&record_text) else {
                continue; // an unreadable message declares nothing
            };
            let role = record.get("role").and_then(|v| v.as_str()).unwrap_or("");
            let run_id = record.get("runId").and_then(|v| v.as_str());
            match role {
                "tool" => {
                    if let (Some(run_id), Some(tool_call_id)) =
                        (run_id, record.get("toolCallId").and_then(|v| v.as_str()))
                    {
                        answered.insert((chat_id, run_id.to_owned(), tool_call_id.to_owned()));
                    }
                }
                "assistant" => {
                    let Some(run_id) = run_id else {
                        continue; // a record without a run cannot own evidence
                    };
                    let Some(calls) = record.get("toolCalls").and_then(|v| v.as_array()) else {
                        continue;
                    };
                    for call in calls {
                        let (Some(id), Some(name)) = (
                            call.get("id").and_then(|v| v.as_str()),
                            call.get("name").and_then(|v| v.as_str()),
                        ) else {
                            continue;
                        };
                        let arguments_json = call
                            .get("argumentsJson")
                            .and_then(|v| v.as_str())
                            .unwrap_or("null");
                        declared.push((
                            chat_id.clone(),
                            run_id.to_owned(),
                            id.to_owned(),
                            name.to_owned(),
                            arguments_json.to_owned(),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    for (chat_id, run_id, tool_call_id, tool_name, arguments_json) in declared {
        if answered.contains(&(chat_id.clone(), run_id.clone(), tool_call_id.clone())) {
            continue;
        }
        // Receipts belong to runs that still exist (FK onto `tasks`); a run
        // whose task row was deleted can never own durable evidence — the
        // admission path could not have recorded it either.
        let task_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?)",
            rusqlite::params![&run_id],
            |row| row.get(0),
        )?;
        if !task_exists {
            continue;
        }
        let arguments = agentkit_contracts::parse_javascript_json(&arguments_json)
            .unwrap_or(serde_json::Value::Null);
        let record = serde_json::json!({
            "executionId": canonical_sha256(&serde_json::json!({
                "runId": run_id,
                "toolCallId": tool_call_id,
                "approvalRequestId": "initial",
            })),
            "runId": run_id,
            "chatId": chat_id,
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "argsSha256": canonical_sha256(&arguments),
            "contextSha256": LEGACY_CONTEXT_SHA256,
            "attemptId": "legacy-upgrade",
            "status": "admitted",
        });
        let execution_id = record["executionId"]
            .as_str()
            .expect("executionId just written")
            .to_owned();
        transaction.execute(
            "INSERT INTO tool_executions(execution_id,run_id,approval_request_id,record) VALUES(?,?,NULL,?) ON CONFLICT(execution_id) DO NOTHING",
            rusqlite::params![execution_id, run_id, record.to_string()],
        )?;
    }
    Ok(())
}

/// Durable agent-session inputs (ADR 0016 §2), additive at schema version 4.
/// The `record` JSON carries everything else; `order` lives in the shared
/// counters table so insertion order is strictly increasing per store.
const SESSIONS_SCHEMA: &str = r#"
CREATE TABLE agent_inputs(input_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, kind TEXT NOT NULL, status TEXT NOT NULL, target_task_id TEXT, record TEXT NOT NULL);
CREATE INDEX agent_inputs_session ON agent_inputs(session_id,status);
CREATE INDEX agent_inputs_target ON agent_inputs(target_task_id);
INSERT INTO counters VALUES('input_order',0);
"#;

fn migrate_sessions(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(SESSIONS_SCHEMA)?;
    transaction.pragma_update(None, "user_version", 4)?;
    transaction.commit()?;
    Ok(())
}

fn schema_signature(connection: &Connection) -> Result<Vec<(String, String, String, String)>> {
    let mut statement = connection.prepare(
        "SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?;
    rows.map(|row| row.map_err(Into::into)).collect()
}

const SCHEMA: &str = r#"
CREATE TABLE tasks(id TEXT PRIMARY KEY, scope TEXT NOT NULL, status TEXT NOT NULL, record TEXT NOT NULL);
CREATE INDEX tasks_scope ON tasks(scope);
CREATE TABLE attempts(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE leases(task_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO counters VALUES('fence',0);
CREATE TABLE events(task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE, seq INTEGER NOT NULL CHECK(seq>=0), event_id TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(task_id,seq), UNIQUE(task_id,event_id));
CREATE TABLE chats(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE messages(id TEXT PRIMARY KEY, chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, parent_id TEXT REFERENCES messages(id), order_key INTEGER NOT NULL, record TEXT NOT NULL, UNIQUE(chat_id,order_key));
CREATE INDEX messages_chat ON messages(chat_id);
CREATE TABLE outbox(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE proposals(id TEXT PRIMARY KEY, scope TEXT NOT NULL, action_id TEXT, status TEXT NOT NULL, operation_id TEXT UNIQUE, record TEXT NOT NULL);
CREATE UNIQUE INDEX proposal_action ON proposals(scope,action_id) WHERE action_id IS NOT NULL AND status NOT IN ('rejected','invalidated');
CREATE TABLE receipts(operation_id TEXT PRIMARY KEY, proposal_id TEXT NOT NULL UNIQUE REFERENCES proposals(id) ON DELETE CASCADE, record TEXT NOT NULL);
CREATE TABLE settings(namespace TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(namespace,key));
CREATE TABLE run_bindings(run_id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE, chat_id TEXT NOT NULL REFERENCES chats(id) ON DELETE CASCADE, scope TEXT NOT NULL, state_required INTEGER NOT NULL DEFAULT 0);
CREATE TABLE continuations(run_id TEXT PRIMARY KEY REFERENCES run_bindings(run_id) ON DELETE CASCADE, anchor_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE, record TEXT NOT NULL);
"#;
