//! Durable derived context masks (ADR 0034, layer 4), keyed globally by mask
//! id. Same settings-namespace shape as context summaries: a mask hides the
//! body of tool results (and old images) in the provider view only; the
//! canonical history is never touched.

use crate::{Result, SqliteStore, StoreError, util::json};
use rusqlite::params;
use serde_json::Value;

pub const CONTEXT_MASKS_NAMESPACE: &str = "agentkit.compat.context-masks";

pub(crate) fn valid(value: &Value) -> bool {
    value.is_object()
        && ["maskId", "sessionId", "sourceHash", "createdAt"]
            .iter()
            .all(|field| value[*field].is_string())
        && value["tokensBefore"].is_number()
        && value["tokensAfter"].is_number()
        && value["coveredCallIds"]
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string))
        && value["masks"].as_array().is_some_and(|list| {
            list.iter().all(|mask| {
                ["messageId", "callId", "handle"]
                    .iter()
                    .all(|field| mask[*field].is_string())
                    && mask["bytes"].is_u64()
            })
        })
        && value["images"].as_array().is_some_and(|list| {
            list.iter().all(|image| {
                image["messageId"].is_string()
                    && image["names"]
                        .as_array()
                        .is_some_and(|names| names.iter().all(Value::is_string))
            })
        })
}
/// This session's mask rows, filtered in SQL. A malformed row is skipped with
/// a warning: one bad record must not hide every other session's masks.
fn rows(db: &rusqlite::Connection, session: &str) -> Result<Vec<(String, Value)>> {
    // CASE keeps `json_extract` away from rows that are not valid JSON (it
    // would fail the whole query); those pass through and are skipped below.
    let mut statement = db.prepare(
        "SELECT key,value FROM settings WHERE namespace=?1 \
         AND CASE WHEN json_valid(value) THEN json_extract(value,'$.sessionId')=?2 ELSE 1 END \
         ORDER BY rowid DESC",
    )?;
    let records = statement.query_map(params![CONTEXT_MASKS_NAMESPACE, session], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = vec![];
    for record in records {
        let (key, raw) = record?;
        let value = agentkit_contracts::parse_javascript_json(&raw)
            .ok()
            .filter(|value| valid(value) && value["maskId"].as_str() == Some(key.as_str()));
        match value {
            Some(value) if value["sessionId"].as_str() == Some(session) => out.push((key, value)),
            Some(_) => {}
            None => eprintln!("agentkit: skipping malformed context mask {key}"),
        }
    }
    Ok(out)
}

/// Removes every mask of `session` on `db`; callers own the transaction.
pub(crate) fn delete_for_session(db: &rusqlite::Connection, session: &str) -> Result<usize> {
    let keys: Vec<_> = rows(db, session)?.into_iter().map(|(key, _)| key).collect();
    for key in &keys {
        db.execute(
            "DELETE FROM settings WHERE namespace=? AND key=?",
            params![CONTEXT_MASKS_NAMESPACE, key],
        )?;
    }
    Ok(keys.len())
}

impl SqliteStore {
    /// A mask id is unique: a second save under the same id is an error, not
    /// a silent loss of the new record.
    pub async fn save_context_mask(&self, record: Value) -> Result<()> {
        if !valid(&record) {
            return Err(StoreError::InvalidInput("Malformed context mask".into()));
        }
        self.call(move |db| {
            let id = record["maskId"].as_str().expect("validated mask");
            let changed = db.execute("INSERT INTO settings(namespace,key,value) VALUES(?,?,?) ON CONFLICT(namespace,key) DO NOTHING", params![CONTEXT_MASKS_NAMESPACE, id, json(db, &record)?])?;
            if changed == 0 {
                return Err(StoreError::InvalidInput(
                    "Context mask id already exists".into(),
                ));
            }
            Ok(())
        }).await
    }
    /// Newest first, like summaries.
    pub async fn list_context_masks(&self, session: &str) -> Result<Vec<Value>> {
        let session = session.to_owned();
        self.call(move |db| {
            let mut records: Vec<_> = rows(db, &session)?
                .into_iter()
                .map(|(_, value)| value)
                .collect();
            records.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
            Ok(records)
        })
        .await
    }
    pub async fn delete_session_context_masks(&self, session: &str) -> Result<usize> {
        let session = session.to_owned();
        self.call(move |db| {
            let tx = crate::transaction::atomic(db)?;
            let count = delete_for_session(&tx, &session)?;
            tx.commit()?;
            Ok(count)
        })
        .await
    }
}
