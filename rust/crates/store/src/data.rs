use crate::{
    util::{all, get, id, json, required},
    *,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

agentkit_contracts::impl_json_via_serde!(OutboxRecord);

mod continuation;
mod continuation_validation;
pub use continuation::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxInput {
    pub id: Option<String>,
    pub topic: String,
    pub run_id: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub payload: Value,
    pub available_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxRecord {
    pub id: String,
    pub topic: String,
    pub run_id: Option<String>,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub payload: Value,
    pub created_at: i64,
    pub available_at: i64,
    pub attempts: u32,
    pub published_at: Option<i64>,
    pub last_error: Option<String>,
    pub claim_token: Option<String>,
    pub claim_owner: Option<String>,
    pub claim_until: Option<i64>,
}

pub(crate) fn enqueue(db: &Connection, input: OutboxInput, now: i64) -> Result<OutboxRecord> {
    let record = OutboxRecord {
        id: input.id.unwrap_or_else(id),
        topic: input.topic,
        run_id: input.run_id,
        payload: input.payload,
        created_at: now,
        available_at: input.available_at,
        attempts: 0,
        published_at: None,
        last_error: None,
        claim_token: None,
        claim_owner: None,
        claim_until: None,
    };
    db.execute(
        "INSERT INTO outbox VALUES(?,?)",
        params![record.id, json(db, &record)?],
    )?;
    Ok(record)
}

fn save(db: &Connection, record: &OutboxRecord) -> Result<()> {
    db.execute(
        "UPDATE outbox SET record=? WHERE id=?",
        params![json(db, record)?, record.id],
    )?;
    Ok(())
}

impl SqliteStore {
    /// Atomically replaces a namespace value only when its committed value still matches.
    pub async fn compare_and_set_setting(
        &self,
        namespace: &str,
        key: &str,
        expected: Option<Value>,
        next: Value,
    ) -> Result<bool> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move|db| {
            let transaction=db.transaction()?;
            let current:Option<String>=transaction.query_row("SELECT value FROM settings WHERE namespace=? AND key=?",params![namespace,key],|row|row.get(0)).optional()?;
            let current=current.map(|value|agentkit_contracts::parse_javascript_json(&value)).transpose()?;
            if current!=expected {return Ok(false);}
            transaction.execute("INSERT INTO settings VALUES(?,?,?) ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value",params![namespace,key,json(&transaction,&next)?])?;
            transaction.commit()?;Ok(true)
        }).await
    }

    /// Atomically binds a namespace key once, accepting only the original value on replay.
    pub async fn bind_setting(&self, namespace: &str, key: &str, value: Value) -> Result<bool> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move |db| {
            let transaction = db.transaction()?;
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT value FROM settings WHERE namespace=? AND key=?",
                    params![namespace, key],
                    |r| r.get(0),
                )
                .optional()?;
            let accepted = match existing {
                Some(existing) => agentkit_contracts::parse_javascript_json(&existing)? == value,
                None => {
                    transaction.execute(
                        "INSERT INTO settings VALUES(?,?,?)",
                        params![namespace, key, json(&transaction, &value)?],
                    )?;
                    true
                }
            };
            transaction.commit()?;
            Ok(accepted)
        })
        .await
    }

    pub async fn enqueue_outbox(&self, input: OutboxInput, now: i64) -> Result<OutboxRecord> {
        self.call(move |db| enqueue(db, input, now)).await
    }

    pub async fn claim_outbox(
        &self,
        owner_id: &str,
        now: i64,
        ttl_ms: i64,
        limit: usize,
        max_attempts: u32,
    ) -> Result<Vec<OutboxRecord>> {
        let owner_id = owner_id.to_owned();
        self.call(move |db| {
            let expires = crate::util::checked_expiry(now, ttl_ms)?;
            if max_attempts == 0 {
                return Err(StoreError::InvalidInput(
                    "max_attempts must be positive".into(),
                ));
            }
            let transaction = db.transaction()?;
            let mut due: Vec<OutboxRecord> = all::<OutboxRecord>(&transaction, "outbox")?
                .into_iter()
                .filter(|r| {
                    r.published_at.is_none()
                        && r.available_at <= now
                        && r.attempts < max_attempts
                        && r.claim_until.is_none_or(|expiry| expiry <= now)
                })
                .collect();
            due.sort_by_key(|r| (r.available_at, r.created_at, r.id.clone()));
            due.truncate(limit.min(10000));
            for record in &mut due {
                record.attempts += 1;
                record.claim_owner = Some(owner_id.clone());
                record.claim_token = Some(id());
                record.claim_until = Some(expires);
                save(&transaction, record)?;
            }
            transaction.commit()?;
            Ok(due)
        })
        .await
    }

    pub async fn publish_outbox(&self, record_id: &str, claim_token: &str, now: i64) -> Result<()> {
        self.finish_outbox(record_id, claim_token, now, None).await
    }

    pub async fn fail_outbox(
        &self,
        record_id: &str,
        claim_token: &str,
        now: i64,
        error: String,
        retry_at: i64,
    ) -> Result<()> {
        self.finish_outbox(record_id, claim_token, now, Some((error, retry_at)))
            .await
    }

    async fn finish_outbox(
        &self,
        record_id: &str,
        token: &str,
        now: i64,
        failure: Option<(String, i64)>,
    ) -> Result<()> {
        let (record_id, token) = (record_id.to_owned(), token.to_owned());
        self.call(move |db| {
            let transaction = db.transaction()?;
            let mut record: OutboxRecord = required(&transaction, "outbox", "id", &record_id)?;
            if record.claim_token.as_deref() != Some(&token)
                || record.claim_until.is_none_or(|expiry| expiry <= now)
            {
                return Err(StoreError::InvalidInput("outbox claim lost".into()));
            }
            if let Some((error, retry)) = failure {
                record.last_error = Some(error);
                record.available_at = retry;
            } else {
                record.published_at = Some(now);
            }
            record.claim_token = None;
            record.claim_owner = None;
            record.claim_until = None;
            save(&transaction, &record)?;
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn outbox(&self) -> Result<Vec<OutboxRecord>> {
        self.call(|db| all(db, "outbox")).await
    }

    pub async fn prune_outbox(&self, before: i64, max_attempts: u32) -> Result<usize> {
        self.call(move |db| {
            if max_attempts == 0 {
                return Err(StoreError::InvalidInput(
                    "max_attempts must be positive".into(),
                ));
            }
            let transaction = db.transaction()?;
            let mut removed = 0;
            for record in all::<OutboxRecord>(&transaction, "outbox")? {
                if record.published_at.is_some_and(|at| at < before)
                    || (record.published_at.is_none()
                        && record.attempts >= max_attempts
                        && record.created_at < before
                        && record.claim_until.is_none_or(|until| until < before))
                {
                    removed += transaction.execute("DELETE FROM outbox WHERE id=?", [record.id])?;
                }
            }
            transaction.commit()?;
            Ok(removed)
        })
        .await
    }

    pub async fn get_setting(&self, namespace: &str, key: &str) -> Result<Option<Value>> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move |db| {
            let value: Option<String> = db
                .query_row(
                    "SELECT value FROM settings WHERE namespace=? AND key=?",
                    params![namespace, key],
                    |r| r.get(0),
                )
                .optional()?;
            value
                .map(|value| Ok(agentkit_contracts::parse_javascript_json(&value)?))
                .transpose()
        })
        .await
    }

    pub async fn list_settings(&self, namespace: &str) -> Result<Vec<(String, Value)>> {
        let namespace = namespace.to_owned();
        self.call(move |db| {
            let mut statement =
                db.prepare("SELECT key,value FROM settings WHERE namespace=? ORDER BY key")?;
            let rows = statement.query_map([namespace], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            rows.map(|row| {
                let (key, value) = row?;
                Ok((key, agentkit_contracts::parse_javascript_json(&value)?))
            })
            .collect()
        })
        .await
    }

    pub async fn set_setting(&self, namespace: &str, key: &str, value: Value) -> Result<()> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move |db| {
            db.execute(
                "INSERT INTO settings VALUES(?,?,?) \
                 ON CONFLICT(namespace,key) DO UPDATE SET value=excluded.value",
                params![namespace, key, json(db, &value)?],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn delete_setting(&self, namespace: &str, key: &str) -> Result<bool> {
        let (namespace, key) = (namespace.to_owned(), key.to_owned());
        self.call(move |db| {
            Ok(db.execute(
                "DELETE FROM settings WHERE namespace=? AND key=?",
                params![namespace, key],
            )? > 0)
        })
        .await
    }
}
