use crate::{Result, SqliteStore, StoreError, util};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct SettingWrite {
    pub namespace: String,
    pub key: String,
    pub value: Option<Value>,
}

#[derive(Debug)]
pub enum SettingsCommit {
    Applied,
    Conflict,
    /// No writes survived, proved by a successful explicit rollback.
    Aborted(StoreError),
}

fn compare_and_write(
    db: &Connection,
    namespace: &str,
    key: &str,
    expected: Option<Value>,
    writes: Vec<SettingWrite>,
) -> Result<bool> {
    let current: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE namespace=? AND key=?",
            params![namespace, key],
            |row| row.get(0),
        )
        .optional()?;
    let current = current
        .map(|value| agentkit_contracts::parse_javascript_json(&value))
        .transpose()?;
    let expected = expected
        .map(|value| {
            agentkit_contracts::parse_javascript_json(&util::json(db, &value)?)
                .map_err(StoreError::from)
        })
        .transpose()?;
    if current != expected {
        return Ok(false);
    }
    for write in writes {
        if let Some(value) = write.value {
            db.execute("INSERT INTO settings VALUES(?,?,?) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value",params![write.namespace,write.key,util::json(db,&value)?])?;
        } else {
            db.execute(
                "DELETE FROM settings WHERE namespace=? AND key=?",
                params![write.namespace, write.key],
            )?;
        }
    }
    Ok(true)
}

fn finish(transaction: Transaction<'_>, result: Result<bool>) -> Result<SettingsCommit> {
    match result {
        Ok(true) => {
            transaction.commit()?;
            Ok(SettingsCommit::Applied)
        }
        Ok(false) => {
            transaction.rollback()?;
            Ok(SettingsCommit::Conflict)
        }
        Err(error) => {
            transaction.rollback()?;
            Ok(SettingsCommit::Aborted(error))
        }
    }
}

impl SqliteStore {
    /// Commits a guarded snapshot and all related settings as one durable unit.
    /// An outer error cannot prove rollback; dropping the future does not cancel queued work.
    pub async fn compare_and_set_settings(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        writes: Vec<SettingWrite>,
    ) -> Result<SettingsCommit> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move |db| {
            let transaction = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = compare_and_write(&transaction, &namespace, &key, expected, writes);
            finish(transaction, result)
        })
        .await
    }
}
