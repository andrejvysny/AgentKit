use crate::{Result, StoreError};
use rusqlite::{Connection, OptionalExtension};
use serde::{Serialize, de::DeserializeOwned};

pub(crate) fn get<T: DeserializeOwned>(
    db: &Connection,
    table: &str,
    key: &str,
    id: &str,
) -> Result<Option<T>> {
    let json: Option<String> = db
        .query_row(
            &format!("SELECT record FROM {table} WHERE {key}=?"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    json.map(|json| serde_json::from_str(&json).map_err(Into::into))
        .transpose()
}

pub(crate) fn required<T: DeserializeOwned>(
    db: &Connection,
    table: &str,
    key: &str,
    id: &str,
) -> Result<T> {
    get(db, table, key, id)?.ok_or_else(|| StoreError::NotFound(id.into()))
}

pub(crate) fn all<T: DeserializeOwned>(db: &Connection, table: &str) -> Result<Vec<T>> {
    let mut statement = db.prepare(&format!("SELECT record FROM {table} ORDER BY rowid"))?;
    let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

pub(crate) fn json<T: Serialize + agentkit_contracts::JsonDeserialize>(
    db: &Connection,
    value: &T,
) -> Result<String> {
    let value = agentkit_contracts::parse_javascript_json(&serde_json::to_string(value)?)?;
    let serialized = if db.path().is_none_or(str::is_empty) {
        serde_json::to_string(&value)?
    } else {
        agentkit_contracts::stringify_javascript_value(&value)?
    };
    // Binary64 rounding can push native integer fields beyond their representable range.
    // Refuse that write before SQL can persist a record this store cannot read.
    let _: T = T::from_json(agentkit_contracts::parse_javascript_json(&serialized)?)?;
    Ok(serialized)
}

pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn sql_integer(value: u64) -> Result<i64> {
    value
        .try_into()
        .map_err(|_| StoreError::InvalidInput("integer exceeds SQLite range".into()))
}

pub(crate) fn safe_integer(value: &serde_json::Value) -> Option<u64> {
    const MAX_SAFE: u64 = 9_007_199_254_740_991;
    if let Some(integer) = value.as_u64() {
        return (integer <= MAX_SAFE).then_some(integer);
    }
    value
        .as_f64()
        .filter(|number| {
            number.is_finite()
                && *number >= 0.0
                && number.fract() == 0.0
                && *number <= MAX_SAFE as f64
        })
        .map(|number| number as u64)
}

pub(crate) fn checked_expiry(now: i64, ttl: i64) -> Result<i64> {
    if ttl <= 0 {
        return Err(StoreError::InvalidInput(
            "lease TTL must be positive".into(),
        ));
    }
    now.checked_add(ttl)
        .ok_or_else(|| StoreError::InvalidInput("lease expiry overflow".into()))
}
