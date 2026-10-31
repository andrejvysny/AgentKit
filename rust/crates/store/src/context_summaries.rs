//! Durable derived context summaries, keyed globally by summary id.

use crate::{Result, SqliteStore, StoreError, util::json};
use rusqlite::params;
use serde_json::Value;

pub const CONTEXT_SUMMARIES_NAMESPACE: &str = "agentkit.compat.context-summaries";

fn valid(value: &Value) -> bool {
    value.is_object()
        && [
            "summaryId",
            "sessionId",
            "coversThroughMessageId",
            "firstKeptMessageId",
            "summary",
            "sourceHash",
            "model",
            "summarizerRevision",
            "createdAt",
        ]
        .iter()
        .all(|field| value[*field].is_string())
        && value["tokensBefore"].is_number()
        && value["tokensAfter"].is_number()
        && value.get("providerId").is_none_or(Value::is_string)
        && value["details"]["readFiles"]
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string))
        && value["details"]["modifiedFiles"]
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string))
}
fn rows(db: &rusqlite::Connection) -> Result<Vec<(String, Value)>> {
    let mut statement =
        db.prepare("SELECT key,value FROM settings WHERE namespace=? ORDER BY rowid DESC")?;
    let records = statement.query_map([CONTEXT_SUMMARIES_NAMESPACE], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    records
        .map(|record| {
            let (key, raw) = record?;
            let value = agentkit_contracts::parse_javascript_json(&raw)
                .map_err(|_| StoreError::RecoveryUnreadable("Malformed context summary".into()))?;
            if !valid(&value) || value["summaryId"].as_str() != Some(key.as_str()) {
                return Err(StoreError::RecoveryUnreadable(
                    "Malformed context summary".into(),
                ));
            }
            Ok((key, value))
        })
        .collect()
}
impl SqliteStore {
    pub async fn save_context_summary(&self, record: Value) -> Result<()> {
        if !valid(&record) {
            return Err(StoreError::InvalidInput("Malformed context summary".into()));
        }
        self.call(move |db| {
            let id = record["summaryId"].as_str().expect("validated summary");
            db.execute("INSERT INTO settings(namespace,key,value) VALUES(?,?,?) ON CONFLICT(namespace,key) DO NOTHING", params![CONTEXT_SUMMARIES_NAMESPACE, id, json(db, &record)?])?;
            Ok(())
        }).await
    }
    pub async fn list_context_summaries(&self, session: &str) -> Result<Vec<Value>> {
        let session = session.to_owned();
        self.call(move |db| {
            let mut records: Vec<_> = rows(db)?
                .into_iter()
                .map(|(_, value)| value)
                .filter(|value| value["sessionId"].as_str() == Some(session.as_str()))
                .collect();
            records.sort_by(|a, b| b["createdAt"].as_str().cmp(&a["createdAt"].as_str()));
            Ok(records)
        })
        .await
    }
    pub async fn delete_session_context_summaries(&self, session: &str) -> Result<usize> {
        let session = session.to_owned();
        self.call(move |db| {
            let tx = crate::transaction::atomic(db)?;
            let keys: Vec<_> = rows(&tx)?
                .into_iter()
                .filter(|(_, value)| value["sessionId"].as_str() == Some(session.as_str()))
                .map(|(key, _)| key)
                .collect();
            for key in &keys {
                tx.execute(
                    "DELETE FROM settings WHERE namespace=? AND key=?",
                    params![CONTEXT_SUMMARIES_NAMESPACE, key],
                )?;
            }
            tx.commit()?;
            Ok(keys.len())
        })
        .await
    }
}
