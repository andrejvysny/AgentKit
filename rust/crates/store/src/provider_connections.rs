//! Durable provider identity and authority. Credential rotation never rewrites
//! admitted run scopes; explicit continuity is limited to settled work.
use crate::{
    Result, StoreError,
    util::{json, safe_integer},
};
use agentkit_kernel::sha256_hex as sha256;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use std::path::Path;

/// Legacy key-only identity, retained solely for explicit legacy restoration.
pub fn provider_connection_identity(connection_key: &str) -> String {
    format!(
        "conn_{}",
        sha256(format!("agentkit-connection-v1\0{connection_key}"))
    )
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidInput(message.into())
}

fn open(path: &Path) -> Result<Connection> {
    if path == Path::new(":memory:") {
        return Err(invalid(
            "a keyed provider connection requires a file-backed store",
        ));
    }
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(|_| {
            invalid("the store must be opened before a keyed provider connection is registered")
        })?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    let identity: i64 = connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if identity != crate::schema::APPLICATION_ID {
        return Err(StoreError::ForeignDatabase);
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != crate::SCHEMA_VERSION {
        return Err(invalid("open the store once to migrate provider authority"));
    }
    Ok(connection)
}

/// Compatibility door for Rust callers without an endpoint. Native transports use
/// scoped registration so endpoint/account changes cannot reuse an identity.
pub fn register_provider_connection(
    path: &Path,
    connection_key: &str,
    api_key: Option<&str>,
    now: i64,
) -> Result<(String, u64)> {
    register_scoped_provider_connection(path, connection_key, api_key, "", "", now)
}

/// Identity includes persistent database namespace and trusted routing scope.
/// Re-registration after revocation and credential rotation advance the fence.
pub fn register_scoped_provider_connection(
    path: &Path,
    connection_key: &str,
    api_key: Option<&str>,
    endpoint: &str,
    account_scope: &str,
    now: i64,
) -> Result<(String, u64)> {
    register_provider_connection_with_continuity(
        path,
        connection_key,
        api_key,
        endpoint,
        account_scope,
        now,
        None,
    )
}

/// Explicit caller confirmation of unchanged endpoint and trusted account.
#[derive(Clone, Debug)]
pub struct ProviderContinuityRestore {
    pub expected_generation: u64,
    pub previous_generation: u64,
    pub legacy: bool,
}

/// Registration and an optional confirmed continuity transfer commit together.
pub fn register_provider_connection_with_continuity(
    path: &Path,
    connection_key: &str,
    api_key: Option<&str>,
    endpoint: &str,
    account_scope: &str,
    now: i64,
    restore: Option<ProviderContinuityRestore>,
) -> Result<(String, u64)> {
    if connection_key.is_empty() || connection_key.len() > 512 || account_scope.len() > 512 {
        return Err(invalid(
            "connectionKey/accountScope must be at most 512 characters and connectionKey must be non-empty",
        ));
    }
    let mut connection = open(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (connection_id, generation) = write_registration(
        &transaction,
        connection_key,
        api_key,
        endpoint,
        account_scope,
        now,
    )?;
    restore_registered(
        &transaction,
        connection_key,
        &connection_id,
        generation,
        account_scope,
        restore,
    )?;
    transaction.commit()?;
    Ok((connection_id, generation))
}

fn write_registration(
    db: &Connection,
    connection_key: &str,
    api_key: Option<&str>,
    endpoint: &str,
    account_scope: &str,
    now: i64,
) -> Result<(String, u64)> {
    let namespace: String = db.query_row(
        "SELECT namespace FROM provider_namespace WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let logical_key = sha256(format!("agentkit-logical-key-v2\0{connection_key}"));
    let material = json!([namespace, connection_key, endpoint, account_scope]).to_string();
    let connection_id = format!("conn2_{}", sha256(material));
    let digest = sha256(format!("agentkit-credential-v1\0{}", api_key.unwrap_or("")));
    let existing: Option<(String, i64, bool)> = db.query_row(
        "SELECT credential_digest,generation,active FROM provider_connections WHERE connection_id=?",
        [&connection_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let generation = match existing {
        None => 1,
        Some((stored, generation, active)) if stored == digest && active => generation,
        Some((_, generation, _)) => generation
            .checked_add(1)
            .filter(|value| *value <= 9_007_199_254_740_991)
            .ok_or_else(|| invalid("provider generation exhausted"))?,
    };
    db.execute(
        "UPDATE provider_connections SET active=0 WHERE logical_key=? AND connection_id<>?",
        params![logical_key, connection_id],
    )?;
    db.execute(
        "INSERT INTO provider_connections(connection_id,credential_digest,generation,updated_at,logical_key,endpoint,account_scope,active)
         VALUES(?,?,?,?,?,?,?,1) ON CONFLICT(connection_id) DO UPDATE SET
         credential_digest=excluded.credential_digest,generation=excluded.generation,updated_at=excluded.updated_at,active=1",
        params![connection_id, digest, generation, now, logical_key, endpoint, account_scope])?;
    Ok((connection_id, generation as u64))
}

fn restore_registered(
    db: &Connection,
    connection_key: &str,
    connection_id: &str,
    generation: u64,
    account_scope: &str,
    restore: Option<ProviderContinuityRestore>,
) -> Result<()> {
    if let Some(restore) = restore {
        if restore.expected_generation != generation || account_scope.is_empty() {
            return Err(invalid("provider_continuation_scope_mismatch"));
        }
        let source_id =
            if restore.legacy {
                let legacy_id = provider_connection_identity(connection_key);
                let legacy: Option<(i64, Option<String>)> = db.query_row(
                "SELECT generation,endpoint FROM provider_connections WHERE connection_id=?",
                [&legacy_id], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
                if legacy != Some((restore.previous_generation as i64, None)) {
                    return Err(invalid("provider_continuation_scope_mismatch"));
                }
                legacy_id
            } else {
                if restore.previous_generation >= restore.expected_generation {
                    return Err(invalid("continuity requires an older generation"));
                }
                connection_id.to_owned()
            };
        transfer_settled_scopes(
            db,
            &source_id,
            connection_id,
            restore.previous_generation,
            restore.expected_generation,
        )?;
    }
    Ok(())
}

/// Durable revocation survives restart; re-registration advances generation.
pub fn revoke_persisted_provider_connection(path: &Path, connection_id: &str) -> Result<bool> {
    let connection = open(path)?;
    Ok(connection.execute(
        "UPDATE provider_connections SET active=0 WHERE connection_id=? AND active=1",
        [connection_id],
    )? != 0)
}

/// Off-thread dispatch fence for keyed native transports.
pub fn check_provider_connection(path: &Path, connection_id: &str, generation: u64) -> Result<()> {
    let connection = open(path)?;
    validate_generation(
        &connection,
        &json!({"connectionId":connection_id,"generation":generation}),
    )
}

/// Runs inside the same transaction as admission or continuation storage.
/// Unknown process-local IDs remain valid; missing durable IDs fail closed.
pub(crate) fn validate_generation(db: &Connection, scope: &Value) -> Result<()> {
    let Some(id) = scope["connectionId"].as_str() else {
        return Ok(());
    };
    let known: Option<(i64, bool, Option<String>)> = db
        .query_row(
            "SELECT generation,active,endpoint FROM provider_connections WHERE connection_id=?",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    match known {
        Some((generation, true, Some(_)))
            if safe_integer(&scope["generation"]) == Some(generation as u64) =>
        {
            Ok(())
        }
        None if !id.starts_with("conn2_") => Ok(()),
        _ => Err(invalid("provider_continuation_scope_mismatch")),
    }
}

/// Compare-and-set continuity transfer after confirming unchanged trusted scope.
/// Only settled compatible bindings move; active invocations never restamp.
pub fn restore_provider_continuity(
    path: &Path,
    connection_id: &str,
    expected_generation: u64,
    previous_generation: u64,
    endpoint: &str,
    account_scope: &str,
) -> Result<usize> {
    if account_scope.is_empty() || previous_generation >= expected_generation {
        return Err(invalid(
            "continuity requires a trusted account and an older generation",
        ));
    }
    let mut connection = open(path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: Option<(i64, String, String)> = transaction.query_row(
        "SELECT generation,endpoint,account_scope FROM provider_connections WHERE connection_id=? AND active=1",
        [connection_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    if current
        != Some((
            expected_generation as i64,
            endpoint.into(),
            account_scope.into(),
        ))
    {
        return Err(invalid("provider_continuation_scope_mismatch"));
    }
    let moved = transfer_settled_scopes(
        &transaction,
        connection_id,
        connection_id,
        previous_generation,
        expected_generation,
    )?;
    transaction.commit()?;
    Ok(moved)
}

fn transfer_settled_scopes(
    db: &Connection,
    id: &str,
    target: &str,
    from: u64,
    to: u64,
) -> Result<usize> {
    let mut statement = db.prepare(
        "SELECT b.run_id,b.scope FROM run_bindings b JOIN tasks t ON t.id=b.run_id
         WHERE t.status IN ('completed','failed','cancelled')",
    )?;
    let rows: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(statement);
    let mut moved = 0;
    for (run_id, scope) in rows {
        let mut scope = agentkit_contracts::parse_javascript_json(&scope)?;
        if scope["connectionId"] != id || safe_integer(&scope["generation"]) != Some(from) {
            continue;
        }
        let record: Option<String> = db
            .query_row(
                "SELECT record FROM continuations WHERE run_id=?",
                [&run_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(record) = record {
            let mut record = agentkit_contracts::parse_javascript_json(&record)?;
            if record["state"]["scope"] != scope {
                continue;
            }
            record["state"]["scope"]["generation"] = json!(to);
            record["state"]["scope"]["connectionId"] = json!(target);
            db.execute(
                "UPDATE continuations SET record=? WHERE run_id=?",
                params![json(db, &record)?, run_id],
            )?;
        }
        scope["generation"] = json!(to);
        scope["connectionId"] = json!(target);
        db.execute(
            "UPDATE run_bindings SET scope=? WHERE run_id=?",
            params![json(db, &scope)?, run_id],
        )?;
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
#[path = "provider_connections_tests.rs"]
mod tests;
