//! Shared plumbing of the SQLite memory adapters (roadmap 3.2, ADR 0035):
//! the fault type that lets SQL helpers use `?` on both rusqlite and
//! memory-port errors, enum wire strings, row mapping and the transaction
//! runners that move a unit of work onto the store's worker.
use crate::{SqliteStore, transaction::atomic};
use agentkit_memory::{
    Actor, EventKind, MemoryError, MemoryEvent, MemoryResult, PurgeReport, Record, RecordId, Scope,
    Timestamp,
};
use rusqlite::{Connection, Row};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

/// What a unit of SQL work can fail with before it is mapped onto the
/// port's own error.
#[derive(Debug)]
pub(crate) enum Fault {
    Memory(MemoryError),
    Sql(rusqlite::Error),
}

impl From<MemoryError> for Fault {
    fn from(error: MemoryError) -> Self {
        Self::Memory(error)
    }
}

impl From<rusqlite::Error> for Fault {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<Fault> for MemoryError {
    fn from(fault: Fault) -> Self {
        match fault {
            Fault::Memory(error) => error,
            Fault::Sql(error) => backend(error),
        }
    }
}

pub(crate) type Work<T> = Result<T, Fault>;

pub(crate) fn backend(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Backend(error.to_string())
}

/// The snake_case wire string of a unit-variant enum.
pub(crate) fn wire<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(text)) => text,
        _ => String::new(),
    }
}

pub(crate) fn unwire<T: DeserializeOwned>(text: &str) -> rusqlite::Result<T> {
    serde_json::from_value(Value::String(text.to_owned())).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn record_id(text: &str) -> rusqlite::Result<RecordId> {
    text.parse::<RecordId>().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn opt_id(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<RecordId>> {
    row.get::<_, Option<String>>(index)?
        .map(|text| record_id(&text))
        .transpose()
}

pub(crate) const RECORD_COLUMNS: &str = "id,scope_kind,scope_id,kind,text,lang,status,origin,\
evidence_quote,evidence_chat_id,evidence_msg_id,evidence_sha256,approved_at,valid_from,valid_to,\
supersedes,superseded_by,is_core,core_order,created_at,created_by_task,last_used_at,use_count";

pub(crate) fn record_from(row: &Row<'_>) -> rusqlite::Result<Record> {
    let ts = |index: usize| -> rusqlite::Result<Option<Timestamp>> {
        Ok(row.get::<_, Option<i64>>(index)?.map(Timestamp))
    };
    Ok(Record {
        id: record_id(&row.get::<_, String>(0)?)?,
        scope: Scope::new(unwire(&row.get::<_, String>(1)?)?, row.get::<_, String>(2)?),
        kind: unwire(&row.get::<_, String>(3)?)?,
        text: row.get(4)?,
        lang: row.get(5)?,
        status: unwire(&row.get::<_, String>(6)?)?,
        origin: unwire(&row.get::<_, String>(7)?)?,
        evidence_quote: row.get(8)?,
        evidence_chat_id: row.get(9)?,
        evidence_msg_id: row.get(10)?,
        evidence_sha256: row.get(11)?,
        approved_at: ts(12)?,
        valid_from: Timestamp(row.get(13)?),
        valid_to: ts(14)?,
        supersedes: opt_id(row, 15)?,
        superseded_by: opt_id(row, 16)?,
        is_core: row.get::<_, i64>(17)? != 0,
        core_order: row.get(18)?,
        created_at: Timestamp(row.get(19)?),
        created_by_task: row.get(20)?,
        last_used_at: ts(21)?,
        use_count: row.get::<_, i64>(22)?.max(0) as u64,
    })
}

pub(crate) fn event_from(row: &Row<'_>) -> rusqlite::Result<MemoryEvent> {
    let detail = row
        .get::<_, Option<String>>(4)?
        .map(|text| serde_json::from_str::<Value>(&text))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
    Ok(MemoryEvent {
        seq: row.get::<_, i64>(0)?.max(0) as u64,
        record_id: record_id(&row.get::<_, String>(1)?)?,
        kind: unwire(&row.get::<_, String>(2)?)?,
        actor: unwire(&row.get::<_, String>(3)?)?,
        detail,
        at: Timestamp(row.get(5)?),
    })
}

/// Append one audit row.
pub(crate) fn append_event(
    conn: &Connection,
    id: RecordId,
    kind: EventKind,
    actor: Actor,
    detail: Option<&Value>,
    at: Timestamp,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO memory_events(record_id,event,actor,detail,at) VALUES(?,?,?,?,?)",
        rusqlite::params![
            id.to_string(),
            wire(&kind),
            wire(&actor),
            detail.map(Value::to_string),
            at.0
        ],
    )?;
    Ok(())
}

/// Read on the store's worker, outside any transaction of ours.
pub(crate) async fn read<T, F>(store: &SqliteStore, work: F) -> MemoryResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Work<T> + Send + 'static,
{
    let outcome = store.call(move |db| Ok(work(db))).await.map_err(backend)?;
    outcome.map_err(MemoryError::from)
}

/// Write on the store's worker in one transaction (a savepoint inside an
/// ambient one): committed when `work` succeeds, rolled back when it fails.
/// Runs under `PRAGMA secure_delete = ON`, so whatever the write deletes or
/// overwrites (a record row, an FTS5 segment merged away) is zeroed in the
/// file rather than left in a page's free space.
pub(crate) async fn write<T, F>(store: &SqliteStore, work: F) -> MemoryResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Work<T> + Send + 'static,
{
    Ok(scrubbed(store, false, work).await?.0)
}

/// [`write`] for a purge: after the commit it checkpoints the WAL (so the
/// earlier page images of the purged content do not linger in the `-wal`
/// file) and records the outcome in the report: an incomplete or failed
/// checkpoint leaves `wal_pages_pending > 0`, so the purge is not clean. The
/// incremental vacuum that follows is best-effort (a no-op unless the
/// database was created with incremental auto-vacuum) and never fails a
/// committed purge.
pub(crate) async fn write_secure<F>(store: &SqliteStore, work: F) -> MemoryResult<PurgeReport>
where
    F: FnOnce(&Connection) -> Work<PurgeReport> + Send + 'static,
{
    let (mut report, checkpoint) = scrubbed(store, true, work).await?;
    if let Some((checkpointed, pending)) = checkpoint {
        report.wal_checkpointed = checkpointed;
        report.wal_pages_pending = pending;
    }
    Ok(report)
}

/// `(complete, pages pending)` of a `wal_checkpoint(TRUNCATE)`: `busy` means
/// a reader blocked it; `log`/`done` are the WAL pages and those folded back
/// (both -1 without a WAL). A failed checkpoint counts as one pending page.
fn checkpoint_outcome(db: &Connection) -> (bool, u64) {
    match db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    }) {
        Ok((busy, log, done)) => {
            let behind = u64::try_from(log - done).unwrap_or(0);
            let pending = if busy != 0 { behind.max(1) } else { behind };
            (pending == 0 && log >= 0, pending)
        }
        Err(_) => (false, 1),
    }
}

/// The checkpoint outcome is `Some` only when `reclaim` ran it.
async fn scrubbed<T, F>(
    store: &SqliteStore,
    reclaim: bool,
    work: F,
) -> MemoryResult<(T, Option<(bool, u64)>)>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Work<T> + Send + 'static,
{
    let outcome = store
        .call(move |db| {
            let previous: i64 = db.pragma_query_value(None, "secure_delete", |row| row.get(0))?;
            db.pragma_update(None, "secure_delete", "ON")?;
            let outcome = (|| {
                let transaction = atomic(db)?;
                match work(&transaction) {
                    Ok(value) => {
                        transaction.commit()?;
                        Ok::<_, rusqlite::Error>(Ok(value))
                    }
                    Err(fault) => Ok(Err(fault)),
                }
            })();
            let restored = db.pragma_update(None, "secure_delete", previous);
            let outcome = outcome?;
            restored?;
            let mut checkpoint = None;
            if reclaim && outcome.is_ok() {
                checkpoint = Some(checkpoint_outcome(db));
                let _ = db.execute_batch("PRAGMA incremental_vacuum;");
            }
            Ok((outcome, checkpoint))
        })
        .await
        .map_err(backend)?;
    let (outcome, checkpoint) = outcome;
    outcome
        .map(|value| (value, checkpoint))
        .map_err(MemoryError::from)
}
