//! Durable logical provider-connection state (P1-E).
//!
//! The binding's connection REGISTRY (keys, base URLs) lives in Rust
//! process memory and is process-local; what must survive a restart is the
//! connection's LOGICAL identity: a non-secret id derived from a
//! caller-supplied `connectionKey`, plus an explicit `generation` that
//! advances on credential rotation. The stored Responses continuation
//! binds `{logical identity, generation}` (see
//! `data/continuation_validation.rs`), so a second application process
//! that presents the same key AND the same credentials resumes the exact
//! same identity — while the scope check itself stays strict: wrong
//! provider, connection, chat, branch or model still rejects before
//! transport.
//!
//! Credential-restoration rules (documented, pinned by
//! `packages/native/tests/provider-continuity.test.ts`):
//! - same `connectionKey` + same credential ⇒ same id AND generation, any
//!   number of restarts (idempotent registration);
//! - same `connectionKey` + different credential ⇒ ROTATION: same id,
//!   generation + 1, every stored continuation/run scope for that logical
//!   connection is restamped to the new generation in the same
//!   transaction, so the continuation continues;
//! - a stale pre-rotation connection object is then rejected at the NEXT
//!   admission (`provider_continuation_scope_mismatch`, host-side) — the
//!   scope check is never weakened and continuation state is never
//!   silently dropped; a late save from a turn admitted BEFORE the
//!   rotation is normalized to the persisted generation rather than
//!   orphaned.
//!
//! The API key never lands here: only its sha-256 digest is stored, and
//! even that exists solely to tell rotation apart from a restart.

use crate::{
    Result, StoreError,
    util::{json, safe_integer},
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::Path;

/// Domain separation for the logical-id derivation: the id is a hash of
/// the caller's `connectionKey` under this fixed tag, so ids can never be
/// forged from other material and key formats can never collide.
const IDENTITY_TAG: &str = "agentkit-connection-v1\0";
/// Domain separation for the stored credential fingerprint.
const CREDENTIAL_TAG: &str = "agentkit-credential-v1\0";
const MAX_KEY_LEN: usize = 512;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256(text: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(text.as_bytes());
    hex(&digest.finalize())
}

/// The durable logical connection id for a caller-supplied key. NEVER
/// derived from the credential, NEVER random: every process that knows
/// the key derives the same id.
pub fn provider_connection_identity(connection_key: &str) -> String {
    format!(
        "conn_{}",
        sha256(&format!("{IDENTITY_TAG}{connection_key}"))
    )
}

fn credential_digest(api_key: Option<&str>) -> String {
    sha256(&format!(
        "{CREDENTIAL_TAG}{}",
        api_key.filter(|key| !key.is_empty()).unwrap_or("")
    ))
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidInput(message.into())
}

/// Register (or re-register) a LOGICAL provider connection in the store at
/// `path`, returning `{logical connection id, generation}`.
///
/// This is the credential-restoration door: it is idempotent for the same
/// credentials and rotates the generation (restamping stored continuation
/// scopes) when the credential changes. It takes no ownership lock — the
/// store's owning handle may be open elsewhere — and never creates a
/// database: registering before the store has ever been opened fails
/// honestly.
pub fn register_provider_connection(
    path: &Path,
    connection_key: &str,
    api_key: Option<&str>,
    now: i64,
) -> Result<(String, u64)> {
    if connection_key.is_empty() || connection_key.len() > MAX_KEY_LEN {
        return Err(invalid(
            "connectionKey must be a non-empty string of at most 512 characters",
        ));
    }
    if path == Path::new(":memory:") {
        return Err(invalid(
            "a keyed provider connection requires a file-backed store",
        ));
    }
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|_| {
            invalid("the store must be opened before a keyed provider connection is registered")
        })?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let identity: i64 = connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if identity != crate::schema::APPLICATION_ID {
        return Err(StoreError::ForeignDatabase);
    }
    let known: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='provider_connections'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if known.is_none() {
        return Err(invalid(
            "the store schema does not carry provider connection state; open the store once to migrate it",
        ));
    }
    let connection_id = provider_connection_identity(connection_key);
    let digest = credential_digest(api_key);
    let transaction = connection.transaction()?;
    let existing: Option<(String, i64)> = transaction
        .query_row(
            "SELECT credential_digest,generation FROM provider_connections WHERE connection_id=?",
            [&connection_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let generation = match existing {
        None => {
            transaction.execute(
                "INSERT INTO provider_connections(connection_id,credential_digest,generation,updated_at) VALUES(?,?,?,?)",
                params![connection_id, digest, 1_i64, now],
            )?;
            1_u64
        }
        Some((stored_digest, stored_generation)) => {
            let generation = if stored_digest == digest {
                stored_generation.max(1) as u64
            } else {
                // Credential rotation under the same logical identity.
                let next = stored_generation.max(1) as u64 + 1;
                transaction.execute(
                    "UPDATE provider_connections SET credential_digest=?,generation=?,updated_at=? WHERE connection_id=?",
                    params![digest, next as i64, now, connection_id],
                )?;
                restamp_scopes(
                    &transaction,
                    &connection_id,
                    stored_generation.max(1) as u64,
                    next,
                )?;
                next
            };
            transaction.execute(
                "UPDATE provider_connections SET updated_at=? WHERE connection_id=?",
                params![now, connection_id],
            )?;
            generation
        }
    };
    transaction.commit()?;
    Ok((connection_id, generation))
}

/// Move every stored scope bound to `{connection_id, from}` onto `to`.
/// Both the run binding and the continuation record move together, in the
/// rotation transaction: continuation state is re-fenced to the live
/// generation, never dropped.
fn restamp_scopes(transaction: &Connection, connection_id: &str, from: u64, to: u64) -> Result<()> {
    let mut bindings = transaction.prepare("SELECT run_id,scope FROM run_bindings")?;
    let rows: Vec<(String, String)> = bindings
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(bindings);
    for (run_id, scope) in rows {
        let mut scope = agentkit_contracts::parse_javascript_json(&scope)?;
        if scope_matches(&scope, connection_id, from) {
            scope["generation"] = json!(to);
            transaction.execute(
                "UPDATE run_bindings SET scope=? WHERE run_id=?",
                params![json(transaction, &scope)?, run_id],
            )?;
        }
    }
    let mut records = transaction.prepare("SELECT run_id,record FROM continuations")?;
    let rows: Vec<(String, String)> = records
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(records);
    for (run_id, record) in rows {
        let mut record = agentkit_contracts::parse_javascript_json(&record)?;
        if scope_matches(&record["state"]["scope"], connection_id, from) {
            record["state"]["scope"]["generation"] = json!(to);
            transaction.execute(
                "UPDATE continuations SET record=? WHERE run_id=?",
                params![json(transaction, &record)?, run_id],
            )?;
        }
    }
    Ok(())
}

fn scope_matches(scope: &Value, connection_id: &str, generation: u64) -> bool {
    scope["connectionId"] == json!(connection_id)
        && safe_integer(&scope["generation"]) == Some(generation)
}

/// The persisted generation of a logical connection on an open store
/// connection; `None` for ids the store has never seen (a keyless,
/// process-local registration — those keep today's strict behaviour).
pub(crate) fn persisted_generation(db: &Connection, connection_id: &str) -> Result<Option<u64>> {
    Ok(db
        .query_row(
            "SELECT generation FROM provider_connections WHERE connection_id=?",
            [connection_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .and_then(|generation| (generation >= 1).then_some(generation as u64)))
}

/// Bind/put time normalization: a keyed connection's scope generation is
/// whatever the store persisted — the persisted row is the authority, so
/// a late save from a pre-rotation connection object re-fences onto the
/// live generation instead of orphaning the state (see the module docs).
/// Keyless ids (no row) are left exactly as provided: the scope check
/// keeps its full strictness for them.
pub(crate) fn normalize_generation(db: &Connection, scope: &mut Value) -> Result<()> {
    let Some(connection_id) = scope["connectionId"].as_str() else {
        return Ok(());
    };
    if let Some(generation) = persisted_generation(db, connection_id)? {
        scope["generation"] = json!(generation);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStore {
        path: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    fn store() -> TempStore {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        // Open once through the real door so schema + ownership stamp land;
        // dropping the handle retires the actor and the ownership lock.
        let store = crate::SqliteStore::open(&path, 64).unwrap();
        drop(store);
        TempStore { path, _dir: dir }
    }

    fn now() -> i64 {
        1_730_000_000_000
    }

    #[test]
    fn identity_is_deterministic_keyed_and_secret_free() {
        let first = provider_connection_identity("openai-main");
        assert_eq!(first, provider_connection_identity("openai-main"));
        assert_ne!(first, provider_connection_identity("openai-backup"));
        assert!(first.starts_with("conn_"));
        // The identity never echoes the key's shape, and credentials are
        // not an input: different keys, different ids; the digest function
        // is separate domain material.
        assert!(!first.contains("openai"));
    }

    #[test]
    fn registration_is_idempotent_for_the_same_credential() {
        let store = store();
        let (id_a, generation_a) =
            register_provider_connection(&store.path, "same", Some("sk-one"), now()).unwrap();
        let (id_b, generation_b) =
            register_provider_connection(&store.path, "same", Some("sk-one"), now()).unwrap();
        assert_eq!(id_a, id_b);
        assert_eq!(generation_a, 1);
        assert_eq!(generation_b, 1);
        // Reopening the file with a fresh handle sees the same state.
        let (id_c, generation_c) =
            register_provider_connection(&store.path, "same", Some("sk-one"), now()).unwrap();
        assert_eq!(id_c, id_a);
        assert_eq!(generation_c, 1);
    }

    #[test]
    fn absent_and_empty_keys_share_one_absent_credential() {
        let store = store();
        let (_, first) = register_provider_connection(&store.path, "keyless", None, now()).unwrap();
        let (id, second) =
            register_provider_connection(&store.path, "keyless", Some(""), now()).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 1);
        assert_eq!(id, provider_connection_identity("keyless"));
    }

    #[test]
    fn rotation_advances_the_generation_under_the_same_identity() {
        let store = store();
        let (id, first) =
            register_provider_connection(&store.path, "rotate", Some("sk-one"), now()).unwrap();
        let (again, second) =
            register_provider_connection(&store.path, "rotate", Some("sk-two"), now()).unwrap();
        assert_eq!(id, again);
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        // Restoring the original credential rotates again — rotation is
        // monotonic, never a rewind to an old fence.
        let (_, third) =
            register_provider_connection(&store.path, "rotate", Some("sk-one"), now()).unwrap();
        assert_eq!(third, 3);
    }

    #[test]
    fn registration_never_creates_a_foreign_or_uninitialized_database() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sqlite");
        assert!(matches!(
            register_provider_connection(&missing, "late", Some("sk"), now()),
            Err(StoreError::InvalidInput(_))
        ));
        let foreign = dir.path().join("foreign.sqlite");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("CREATE TABLE t(x);")
            .unwrap();
        assert!(matches!(
            register_provider_connection(&foreign, "late", Some("sk"), now()),
            Err(StoreError::ForeignDatabase)
        ));
        assert!(matches!(
            register_provider_connection(Path::new(":memory:"), "late", Some("sk"), now()),
            Err(StoreError::InvalidInput(_))
        ));
        assert!(matches!(
            register_provider_connection(&store().path, "", Some("sk"), now()),
            Err(StoreError::InvalidInput(_))
        ));
    }

    #[test]
    fn rotation_restamps_stored_continuation_and_run_scopes() {
        let store = store();
        let (connection_id, generation) =
            register_provider_connection(&store.path, "restamp", Some("sk-one"), now()).unwrap();
        assert_eq!(generation, 1);
        // Seed a bound run + stored continuation under generation 1.
        let scope = |generation: u64| {
            json!({"providerId":"p","protocol":"responses","connectionId":connection_id,
                   "generation":generation,"chatId":"chat","branchId":"anchor","model":"m"})
        };
        let db = Connection::open(&store.path).unwrap();
        db.execute(
            "INSERT INTO tasks VALUES('run-1','chat','pending','{}')",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO chats VALUES('chat','{}')", [])
            .unwrap();
        db.execute(
            "INSERT INTO messages VALUES('anchor','chat',NULL,1,'{}')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO run_bindings VALUES('run-1','chat',?,1)",
            [json(&db, &scope(1)).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO continuations VALUES('run-1','anchor',?)",
            [json(
                &db,
                &json!({"runId":"run-1","anchorMessageId":"anchor",
                        "state":{"version":1,"scope":scope(1),"messageCount":2,
                                 "messageDigest":"0".repeat(64),"inputItems":[]}}),
            )
            .unwrap()],
        )
        .unwrap();
        drop(db);
        // Rotate: both persisted scopes move to generation 2 together.
        let (_, next) =
            register_provider_connection(&store.path, "restamp", Some("sk-two"), now()).unwrap();
        assert_eq!(next, 2);
        let db = Connection::open(&store.path).unwrap();
        let bound: String = db
            .query_row(
                "SELECT scope FROM run_bindings WHERE run_id='run-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&bound).unwrap()["generation"],
            2
        );
        let record: String = db
            .query_row(
                "SELECT record FROM continuations WHERE run_id='run-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let record = serde_json::from_str::<Value>(&record).unwrap();
        assert_eq!(record["state"]["scope"]["generation"], 2);
        assert_eq!(record["state"]["inputItems"], json!([]));
        assert_eq!(persisted_generation(&db, &connection_id).unwrap(), Some(2));
        // A second rotation moves the scopes again; restoring the old
        // credential rotates forward — the generation never rewinds.
        let (same_id, third) =
            register_provider_connection(&store.path, "restamp", Some("sk-one"), now()).unwrap();
        assert_eq!(same_id, connection_id);
        assert_eq!(third, 3);
        let bound: String = db
            .query_row(
                "SELECT scope FROM run_bindings WHERE run_id='run-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&bound).unwrap()["generation"],
            3
        );
        // Unknown ids keep their strict, unnormalized shape.
        let mut foreign_scope = json!({"connectionId": "conn_never_registered", "generation": 7});
        normalize_generation(&db, &mut foreign_scope).unwrap();
        assert_eq!(foreign_scope["generation"], 7);
    }
}
