use crate::{Result, StoreError};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub(crate) const APPLICATION_ID: i64 = 0x414b5253;
/// The schema version this build creates and accepts; migrations land as
/// `version < N` steps below. Version 8 adds the durable uncertainty
/// barriers a tolerant TypeScript import leaves for admitted effects whose
/// receipt cannot be placed; version 9 adds the persistent FTS5 message
/// index (with a one-time backfill of the messages already on disk).
pub const SCHEMA_VERSION: i64 = 9;
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
    if !fresh && !(1..=VERSION).contains(&version) {
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
        if version < 5 {
            migrate_free_receipts(&mut connection)?;
        }
        if version < 6 {
            migrate_provider_connections(&mut connection)?;
        }
        if version < 7 {
            migrate_managed_content(&mut connection)?;
        }
        if version < 8 {
            migrate_barriers(&mut connection)?;
        }
        if version < 9 {
            migrate_message_search(&mut connection)?;
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
    // A v1..v4 database legitimately still carries the proposal-bound
    // `receipts.proposal_id`; `migrate_free_receipts` relaxes it below, so the
    // signature comparison normalizes that one column away on both sides.
    let normalize =
        |rows: Vec<(String, String, String, String)>| -> Vec<(String, String, String, String)> {
            rows.into_iter()
            .map(|(kind, name, table, sql)| {
                (
                    kind,
                    name,
                    table,
                    sql.replace(
                        "proposal_id TEXT NOT NULL UNIQUE REFERENCES proposals(id) ON DELETE CASCADE",
                        "proposal_id TEXT UNIQUE REFERENCES proposals(id) ON DELETE CASCADE",
                    ),
                )
            })
            .collect()
        };
    if normalize(schema_signature(connection)?) != normalize(schema_signature(&expected)?) {
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
    if version >= 5 {
        // Port-compat at version 5: a receipt may exist without a proposal
        // (the port's `recordOutcome` keys by operation id alone), so the
        // column is nullable — NULL passes the foreign-key check honestly.
        sql = sql.replace(
            "proposal_id TEXT NOT NULL UNIQUE REFERENCES proposals(id) ON DELETE CASCADE",
            "proposal_id TEXT UNIQUE REFERENCES proposals(id) ON DELETE CASCADE",
        );
    }
    if version >= 6 {
        sql.push_str(CONNECTIONS_SCHEMA);
    }
    if version >= 7 {
        sql.push_str(MANAGED_CONTENT_SCHEMA);
    }
    if version >= 8 {
        sql.push_str(BARRIERS_SCHEMA);
    }
    if version >= 9 {
        sql.push_str(&message_search_schema());
    }
    sql
}

/// Durable provider-connection state (P1-E), additive at schema version 6.
/// One row per LOGICAL provider connection registered with a durable
/// `connectionKey`: the credential is kept only as a sha-256 digest
/// (rotation detection — never the secret), and `generation` advances on
/// every rotation so the continuation scope binds
/// `{logical identity, generation}` across process restarts.
const CONNECTIONS_SCHEMA: &str = r#"
CREATE TABLE provider_connections(connection_id TEXT PRIMARY KEY, credential_digest TEXT NOT NULL, generation INTEGER NOT NULL CHECK(generation>=1), updated_at INTEGER NOT NULL);
"#;

fn migrate_provider_connections(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(CONNECTIONS_SCHEMA)?;
    transaction.pragma_update(None, "user_version", 6)?;
    transaction.commit()?;
    Ok(())
}

/// Managed content (the managed-integrations recipe's backend), additive at
/// schema version 7. The template/connection tables mirror the shared
/// TypeScript adapters one-to-one (`prompt_templates` keeps the tombstoned
/// primary key that permanently reserves an immutable revision;
/// `connector_connections` keeps `revision` beside the record for the
/// compare-and-swap write). `managed_skills` stores the published skill
/// documents under their content-addressed revision, and
/// `run_content_records` persists the `{skill, path, revision}` references
/// the activation flow records, in insertion order. No table here carries a
/// secret: connection reads redact credential-shaped transport values and
/// `secretRefs` entries are refused at write time.
const MANAGED_CONTENT_SCHEMA: &str = r#"
CREATE TABLE prompt_templates(id TEXT NOT NULL, revision TEXT NOT NULL, template TEXT NOT NULL, PRIMARY KEY(id, revision));
CREATE TABLE connector_connections(id TEXT PRIMARY KEY, revision INTEGER NOT NULL CHECK(revision>=1), record TEXT NOT NULL);
CREATE TABLE managed_skills(source_id TEXT NOT NULL, path TEXT NOT NULL, revision TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(source_id, path));
CREATE TABLE run_content_records(id INTEGER PRIMARY KEY, record TEXT NOT NULL);
"#;

fn migrate_managed_content(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(MANAGED_CONTENT_SCHEMA)?;
    transaction.pragma_update(None, "user_version", 7)?;
    transaction.commit()?;
    Ok(())
}

/// Durable uncertainty barriers (ADR 0020), additive at schema version 8.
/// When the tolerant TypeScript import cannot place an admitted-without-
/// result receipt — its chat cannot be proven, its record is corrupt, its
/// run was never imported — warning-plus-omission would silently delete
/// the evidence that an external effect MIGHT have happened, and the
/// identical mutation could run a second time. Instead the import writes
/// one barrier row keyed by the tool name plus the best-known identity
/// (source execution id, run, argument fingerprint under its provenance);
/// the host guard and the store's admission gate treat every barrier as an
/// unresolved uncertainty until a human reauthorizes its exact id. No
/// foreign key: a barrier may legitimately outlive a run that was never
/// imported.
const BARRIERS_SCHEMA: &str = r#"
CREATE TABLE uncertainty_barriers(barrier_id TEXT PRIMARY KEY, chat_id TEXT, run_id TEXT, tool_name TEXT NOT NULL, args_sha256 TEXT, args_hash_provenance TEXT NOT NULL DEFAULT 'canonical', reason TEXT NOT NULL);
"#;

fn migrate_barriers(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(BARRIERS_SCHEMA)?;
    transaction.pragma_update(None, "user_version", 8)?;
    transaction.commit()?;
    Ok(())
}

/// The searchable projection of one message row, as SQL — the mirror of
/// the TypeScript adapters' `messageSearchText`, which read a `content`
/// column while the Rust store keeps the whole record as JSON, so this
/// reaches into `record` with json1: a string body as itself, a parts body
/// as ALL of its `{"type":"text"}` parts joined by newlines (non-text
/// parts contribute nothing; base64 image bytes are not prose), anything
/// else as empty. ONE definition, spliced into the view and every trigger:
/// an index and a delete trigger that disagree about a message's text
/// would leave the index quietly wrong, detectable only by searching for a
/// term that used to be there. `record` is the qualified row reference —
/// `m.record`, `new.record`, `old.record`.
fn message_search_text(record: &str) -> String {
    format!(
        r#"CASE json_type({record}, '$.content')
             WHEN 'text' THEN json_extract({record}, '$.content')
             WHEN 'array' THEN COALESCE((SELECT group_concat(
                 CASE WHEN json_type(part.value, '$.text') = 'text'
                      THEN json_extract(part.value, '$.text') END, char(10))
               FROM json_each({record}, '$.content') AS part
              WHERE json_extract(part.value, '$.type') = 'text'), '')
             ELSE '' END"#
    )
}

/// The persistent message search index — the TypeScript `message_search`
/// FTS5 index's Rust counterpart, additive at schema version 9. Search
/// used to rebuild a temporary FTS5 table from the whole message corpus on
/// EVERY query — one read scan, deserialize, CREATE, INSERT loop, MATCH
/// and DROP per request. The index now lives in the schema and is the
/// authoritative search predicate: `search_messages` answers
/// `message_search MATCH` under `unicode61 remove_diacritics 2` with bm25
/// ranking, exactly as the TypeScript adapters did.
///
/// The layout is the TypeScript one, one-to-one: a VIEW computing the
/// searchable text of every message, an EXTERNAL-CONTENT FTS5 table over
/// it (the index holds terms and postings, never a second copy of the
/// text), and THREE triggers, because with external content nothing
/// maintains the index on its own. A `'delete'` command must carry the OLD
/// text so FTS5 removes exactly the postings it inserted — which is why the
/// projection is repeated in the trigger bodies rather than re-read through
/// the view, whose row is already gone by then. Every message write — the
/// store's append/update paths, the history import, the TypeScript import's
/// direct INSERT ... ON CONFLICT, and the FK cascade of a chat deletion —
/// flows through these triggers, so no writer can leave the index behind.
///
/// The corpus is EVERY message: visibility (`archived`, `metadata.internal`,
/// `metadata.placeholder`) is decided at QUERY time, in the join back to
/// `messages`, exactly where the TypeScript adapter keeps its filter —
/// those metadata keys are rewritten after indexing (a placeholder becomes
/// a real answer the moment its run finishes), and an index that had
/// decided membership at insert time would keep every finished answer
/// permanently unfindable.
///
/// THE INDEX IS KEYED BY `messages.rowid`, SO DO NOT VACUUM A LIVE STORE
/// FILE: `messages` has a TEXT primary key, its rowid is SQLite's own, and
/// VACUUM may renumber it without telling FTS5 — every posting would
/// silently name a different message.
fn message_search_schema() -> String {
    let view = message_search_text("m.record");
    let new = message_search_text("new.record");
    let old = message_search_text("old.record");
    format!(
        r#"
CREATE VIEW message_search_source AS
SELECT m.rowid AS rowid, {view} AS body FROM messages AS m;
CREATE VIRTUAL TABLE message_search USING fts5(
  body,
  content='message_search_source',
  content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);
CREATE TRIGGER messages_search_insert AFTER INSERT ON messages BEGIN
  INSERT INTO message_search(rowid, body) VALUES (new.rowid, {new});
END;
CREATE TRIGGER messages_search_delete AFTER DELETE ON messages BEGIN
  INSERT INTO message_search(message_search, rowid, body)
    VALUES ('delete', old.rowid, {old});
END;
CREATE TRIGGER messages_search_update AFTER UPDATE ON messages BEGIN
  INSERT INTO message_search(message_search, rowid, body)
    VALUES ('delete', old.rowid, {old});
  INSERT INTO message_search(rowid, body) VALUES (new.rowid, {new});
END;
-- Backfill, guarded so re-applying this DDL cannot double-index anything.
-- The guard reads the %_docsize SHADOW table rather than message_search
-- itself: scanning an external-content table without a MATCH iterates the
-- CONTENT SOURCE, so `SELECT 1 FROM message_search` is non-empty whenever
-- `messages` is — the guard would read "already populated" on a completely
-- empty index and skip the backfill it exists to perform. On a fresh
-- database this inserts nothing; it earns its place when a pre-v9 file
-- (whose rows predate the index) is migrated.
INSERT INTO message_search(rowid, body)
  SELECT source.rowid, source.body FROM message_search_source AS source
   WHERE NOT EXISTS (SELECT 1 FROM message_search_docsize);
"#
    )
}

fn migrate_message_search(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(&message_search_schema())?;
    transaction.pragma_update(None, "user_version", 9)?;
    transaction.commit()?;
    Ok(())
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

/// Version 5: receipt rows are no longer proposal-bound — the port's
/// `recordOutcome` dedups by operation id alone, so `proposal_id` becomes
/// nullable (NULL passes the foreign key honestly; any legacy empty-string
/// sentinel from the transition window is normalized to NULL).
fn migrate_free_receipts(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction()?;
    let current = match transaction.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='receipts'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        Ok(sql) => Some(sql),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error.into()),
    };
    if current.is_some_and(|sql| sql.contains("NOT NULL UNIQUE REFERENCES proposals")) {
        // Column redefine via the empty-orphan pattern: a table rebuild that
        // renames a foreign-key parent needs `legacy_alter_table`, so the
        // rows are parked in a plain scratch table instead. The scratch lives
        // only inside this transaction; a failure below rolls the whole
        // migration back (user_version included) and leaves `receipts`
        // exactly as it was.
        transaction.execute_batch("CREATE TABLE receipts_copy(operation_id TEXT PRIMARY KEY, proposal_id TEXT, record TEXT NOT NULL);")?;
        transaction.execute_batch("INSERT INTO receipts_copy SELECT operation_id, NULLIF(proposal_id,''), record FROM receipts;")?;
        transaction.execute_batch("DROP TABLE receipts;")?;
        transaction.execute_batch("CREATE TABLE receipts(operation_id TEXT PRIMARY KEY, proposal_id TEXT UNIQUE REFERENCES proposals(id) ON DELETE CASCADE, record TEXT NOT NULL);")?;
        transaction.execute_batch("INSERT INTO receipts SELECT operation_id, proposal_id, record FROM receipts_copy; DROP TABLE receipts_copy;")?;
    }
    transaction.pragma_update(None, "user_version", 5)?;
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

pub(crate) fn sha256_hex(text: &str) -> String {
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

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
