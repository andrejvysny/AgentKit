//! The SQLite store as the memory context's `MemoryRepository` (roadmap 3.2,
//! ADR 0035).
//!
//! A [`MemoryTx`] cannot hold a connection across awaits (every database
//! job runs on the store's worker), so it stages: write calls validate
//! against the committed state overlaid with the transaction's own earlier
//! writes and queue ops, and `commit` replays the queue in one SQLite
//! transaction. A replayed op re-checks its precondition in SQL, so a
//! record that moved since the call surfaces as [`MemoryError::Conflict`]
//! with nothing applied. A dropped handle never touched the database.
//!
//! The repository and the retrieval index of one store share the schema, so
//! [`MemoryRepository::commit_with_index`] applies a record transaction and
//! an index transaction as one SQLite transaction.
use crate::memory_codec::{
    RECORD_COLUMNS, Work, append_event, event_from, read, record_from, wire, write, write_secure,
};
use crate::memory_index::{self, IndexOp, apply_index_ops};
use crate::{SqliteStore, memory_purge};
use agentkit_memory::{
    Actor, CorePin, EventKind, IndexTx, MemoryError, MemoryEvent, MemoryRepository, MemoryResult,
    MemoryTx, NewRecord, Page, PurgeReport, Record, RecordFilter, RecordId, RetrievalIndex, Scope,
    Status, Timestamp, core_tokens,
};
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as Sql};
use serde_json::json;
use std::collections::HashMap;

/// One staged write, replayed at commit.
enum Op {
    Insert {
        id: RecordId,
        rec: Box<NewRecord>,
        supersedes: Option<RecordId>,
    },
    /// Close the replaced record of a supersession.
    Close {
        id: RecordId,
        at: Timestamp,
        by: RecordId,
        actor: Actor,
    },
    Status {
        id: RecordId,
        from: Status,
        to: Status,
        actor: Actor,
        at: Timestamp,
    },
    /// Pin or unpin a current active record.
    Core {
        id: RecordId,
        core: bool,
        order: Option<i64>,
        /// Ceiling on the block's tokens, re-checked in the commit.
        max_tokens: Option<usize>,
        actor: Actor,
        at: Timestamp,
    },
}

/// A record transaction: queued ops and the records they changed, which
/// later calls of the same transaction see.
pub(crate) struct Staged {
    ops: Vec<Op>,
    overlay: HashMap<RecordId, Record>,
}

impl Staged {
    fn stage_insert(&mut self, id: RecordId, rec: NewRecord, supersedes: Option<RecordId>) {
        let mut record = rec.clone().into_record(id);
        record.supersedes = supersedes;
        self.overlay.insert(id, record);
        self.ops.push(Op::Insert {
            id,
            rec: Box::new(rec),
            supersedes,
        });
    }
}

/// `MemoryRepository` over a [`SqliteStore`].
#[derive(Clone)]
pub struct SqliteMemoryRepository {
    store: SqliteStore,
}

impl SqliteMemoryRepository {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    async fn committed(&self, id: RecordId) -> MemoryResult<Option<Record>> {
        read(&self.store, move |conn| fetch(conn, id)).await
    }

    /// The record as this transaction sees it.
    async fn visible(&self, staged: &Staged, id: RecordId) -> MemoryResult<Option<Record>> {
        match staged.overlay.get(&id) {
            Some(record) => Ok(Some(record.clone())),
            None => self.committed(id).await,
        }
    }
}

fn fetch(conn: &Connection, id: RecordId) -> Work<Option<Record>> {
    let sql = format!("SELECT {RECORD_COLUMNS} FROM memory_records WHERE id=?");
    Ok(conn
        .query_row(&sql, [id.to_string()], record_from)
        .optional()?)
}

fn conflict(what: &str, id: RecordId) -> MemoryError {
    MemoryError::Conflict(format!("record {id} {what} since the transaction began"))
}

/// The block of the owner's scope and `id`'s scope, with `id` pinned, must
/// fit `max` tokens; read in the commit's own transaction so a concurrent
/// pin cannot slip past a check made earlier.
fn check_core_budget(conn: &Connection, id: RecordId, max: usize) -> Work<()> {
    let Some(record) = fetch(conn, id)? else {
        return Err(MemoryError::NotFound(id).into());
    };
    let mut scopes = vec![Scope::user()];
    if record.scope != Scope::user() {
        scopes.push(record.scope);
    }
    let (sql, args) = core_sql(&scopes);
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(args), record_from)?;
    let pinned = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let used = core_tokens(&pinned);
    if used > max {
        return Err(MemoryError::InvalidInput(format!(
            "pinning would use {used} of the {max} core-block tokens; unpin or shorten an entry first"
        ))
        .into());
    }
    Ok(())
}

fn replay(conn: &Connection, ops: Vec<Op>) -> Work<()> {
    for op in ops {
        match op {
            Op::Insert {
                id,
                rec,
                supersedes,
            } => {
                if let Some(old) = supersedes {
                    let alive: bool = conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM memory_records WHERE id=?)",
                        [old.to_string()],
                        |row| row.get(0),
                    )?;
                    if !alive {
                        return Err(conflict("was purged", old).into());
                    }
                }
                insert_row(conn, id, &rec, supersedes)?;
            }
            Op::Close { id, at, by, actor } => {
                let rows = conn.execute(
                    "UPDATE memory_records SET valid_to=?1, superseded_by=?2 \
                     WHERE id=?3 AND status='active' AND valid_to IS NULL",
                    params![at.0, by.to_string(), id.to_string()],
                )?;
                if rows == 0 {
                    return Err(conflict("was replaced or retracted", id).into());
                }
                let detail = json!({ "by": by.to_string() });
                append_event(conn, id, EventKind::Superseded, actor, Some(&detail), at)?;
            }
            Op::Status {
                id,
                from,
                to,
                actor,
                at,
            } => {
                let rows = conn.execute(
                    "UPDATE memory_records SET status=?1, \
                     approved_at=CASE WHEN ?1='active' THEN ?2 ELSE approved_at END, \
                     valid_to=CASE WHEN ?1='retracted' AND valid_to IS NULL THEN ?2 ELSE valid_to END \
                     WHERE id=?3 AND status=?4",
                    params![wire(&to), at.0, id.to_string(), wire(&from)],
                )?;
                if rows == 0 {
                    return Err(conflict("changed status", id).into());
                }
                // Only active records are indexed: leaving `active` drops the
                // index row in this same transaction.
                if from == Status::Active && to != Status::Active {
                    crate::memory_vectors::delete_item(conn, &id.to_string())?;
                    conn.execute(
                        "DELETE FROM memory_index_items WHERE item_id=?",
                        [id.to_string()],
                    )?;
                }
                append_event(conn, id, Status::event_into(to), actor, None, at)?;
            }
            Op::Core {
                id,
                core,
                order,
                max_tokens,
                actor,
                at,
            } => {
                let rows = conn.execute(
                    "UPDATE memory_records SET is_core=?1, core_order=?2 \
                     WHERE id=?3 AND status='active' AND valid_to IS NULL",
                    params![i64::from(core), order, id.to_string()],
                )?;
                if rows == 0 {
                    return Err(conflict("stopped being current and active", id).into());
                }
                if let (true, Some(max)) = (core, max_tokens) {
                    check_core_budget(conn, id, max)?;
                }
                let kind = if core {
                    EventKind::Pinned
                } else {
                    EventKind::Unpinned
                };
                append_event(conn, id, kind, actor, None, at)?;
            }
        }
    }
    Ok(())
}

fn insert_row(
    conn: &Connection,
    id: RecordId,
    rec: &NewRecord,
    supersedes: Option<RecordId>,
) -> Work<()> {
    conn.execute(
        "INSERT INTO memory_records(id,scope_kind,scope_id,kind,text,lang,status,origin,\
         evidence_quote,evidence_chat_id,evidence_msg_id,evidence_sha256,approved_at,valid_from,\
         supersedes,is_core,core_order,created_at,created_by_task) \
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            id.to_string(),
            wire(&rec.scope.kind),
            rec.scope.id,
            wire(&rec.kind),
            rec.text,
            rec.lang,
            wire(&rec.status),
            wire(&rec.origin),
            rec.evidence_quote,
            rec.evidence_chat_id,
            rec.evidence_msg_id,
            rec.evidence_sha256,
            rec.approved_at.map(|t| t.0),
            rec.valid_from.0,
            supersedes.map(|s| s.to_string()),
            i64::from(rec.is_core),
            rec.core_order,
            rec.created_at.0,
            rec.created_by_task,
        ],
    )?;
    let kind = if rec.status == Status::Active {
        EventKind::Captured
    } else {
        EventKind::Proposed
    };
    let mut detail = serde_json::Map::new();
    if let Some(old) = supersedes {
        detail.insert("supersedes".to_owned(), json!(old.to_string()));
    }
    if let Some(why) = &rec.rationale {
        detail.insert("rationale".to_owned(), json!(why));
    }
    let detail = (!detail.is_empty()).then(|| serde_json::Value::Object(detail));
    append_event(
        conn,
        id,
        kind,
        rec.origin.actor(),
        detail.as_ref(),
        rec.created_at,
    )?;
    Ok(())
}

/// Reject a replacement that cannot take over `current`.
fn check_supersede(current: &Record, new: &NewRecord) -> MemoryResult<()> {
    if current.status != Status::Active || current.valid_to.is_some() {
        return Err(MemoryError::InvalidTransition {
            from: current.status,
            to: Status::Active,
        });
    }
    if current.scope != new.scope {
        return Err(MemoryError::InvalidInput(
            "a record supersedes only within its scope".to_owned(),
        ));
    }
    if new.valid_from < current.valid_from {
        return Err(MemoryError::InvalidInput(
            "a replacement cannot start before the record it replaces".to_owned(),
        ));
    }
    Ok(())
}

fn staged(tx: &mut MemoryTx) -> MemoryResult<&mut Staged> {
    tx.state_mut::<Staged>()
}

fn scope_clause(scopes: &[Scope], args: &mut Vec<Sql>) -> String {
    let each = scopes.iter().map(|scope| {
        args.push(Sql::Text(wire(&scope.kind)));
        args.push(Sql::Text(scope.id.clone()));
        "(scope_kind=? AND scope_id=?)"
    });
    format!("({})", each.collect::<Vec<_>>().join(" OR "))
}

fn core_sql(scopes: &[Scope]) -> (String, Vec<Sql>) {
    let mut args = Vec::new();
    let clause = scope_clause(scopes, &mut args);
    let sql = format!(
        "SELECT {RECORD_COLUMNS} FROM memory_records \
         WHERE is_core=1 AND status='active' AND valid_to IS NULL AND {clause} \
         ORDER BY CASE scope_kind WHEN 'user' THEN 0 WHEN 'project' THEN 1 ELSE 2 END, \
         core_order IS NULL, core_order, id"
    );
    (sql, args)
}

fn list_sql(filter: &RecordFilter, page: Page) -> (String, Vec<Sql>) {
    let mut args = Vec::new();
    let mut clauses = Vec::new();
    if !filter.scopes.is_empty() {
        clauses.push(scope_clause(&filter.scopes, &mut args));
    }
    if !filter.kinds.is_empty() {
        let marks = vec!["?"; filter.kinds.len()].join(",");
        clauses.push(format!("kind IN ({marks})"));
        args.extend(filter.kinds.iter().map(|k| Sql::Text(wire(k))));
    }
    if !filter.statuses.is_empty() {
        let marks = vec!["?"; filter.statuses.len()].join(",");
        clauses.push(format!("status IN ({marks})"));
        args.extend(filter.statuses.iter().map(|s| Sql::Text(wire(s))));
    }
    if let Some(core) = filter.is_core {
        clauses.push("is_core=?".to_owned());
        args.push(Sql::Integer(i64::from(core)));
    }
    if !filter.include_superseded {
        match filter.as_of {
            Some(at) => {
                clauses.push("valid_from<=? AND (valid_to IS NULL OR valid_to>?)".to_owned());
                args.extend([Sql::Integer(at.0), Sql::Integer(at.0)]);
            }
            None => clauses.push("valid_to IS NULL".to_owned()),
        }
    }
    let wher = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    args.extend([
        Sql::Integer(i64::from(page.limit)),
        Sql::Integer(i64::from(page.offset)),
    ]);
    (
        format!(
            "SELECT {RECORD_COLUMNS} FROM memory_records{wher} \
             ORDER BY created_at, id LIMIT ? OFFSET ?"
        ),
        args,
    )
}

#[async_trait]
impl MemoryRepository for SqliteMemoryRepository {
    async fn begin(&self) -> MemoryResult<MemoryTx> {
        Ok(MemoryTx::new(Staged {
            ops: Vec::new(),
            overlay: HashMap::new(),
        }))
    }

    async fn commit(&self, tx: MemoryTx) -> MemoryResult<()> {
        let ops = tx.into_state::<Staged>()?.ops;
        if ops.is_empty() {
            return Ok(());
        }
        write(&self.store, move |conn| replay(conn, ops)).await
    }

    async fn commit_with_index(
        &self,
        tx: MemoryTx,
        index: &dyn RetrievalIndex,
        mut index_tx: IndexTx,
    ) -> MemoryResult<()> {
        let ours = index_tx
            .state_mut::<memory_index::Staged>()
            .is_ok_and(|staged| staged.same_store(&self.store));
        if !ours {
            self.commit(tx).await?;
            return index.commit_index(index_tx).await;
        }
        let ops = tx.into_state::<Staged>()?.ops;
        let index_ops: Vec<IndexOp> = index_tx.into_state::<memory_index::Staged>()?.into_ops();
        write(&self.store, move |conn| {
            replay(conn, ops)?;
            apply_index_ops(conn, index_ops)
        })
        .await
    }

    async fn insert(&self, tx: &mut MemoryTx, rec: NewRecord) -> MemoryResult<RecordId> {
        rec.validate()?;
        let id = RecordId::new();
        staged(tx)?.stage_insert(id, rec, None);
        Ok(id)
    }

    async fn supersede(
        &self,
        tx: &mut MemoryTx,
        old: RecordId,
        new: NewRecord,
    ) -> MemoryResult<RecordId> {
        if new.status != Status::Active {
            return Err(MemoryError::InvalidInput(
                "a replacement record must be active".to_owned(),
            ));
        }
        let staged = staged(tx)?;
        let current = self
            .visible(staged, old)
            .await?
            .ok_or(MemoryError::NotFound(old))?;
        check_supersede(&current, &new)?;
        new.validate()?;
        let (closed_at, actor) = (new.valid_from, new.origin.actor());
        let new_id = RecordId::new();
        staged.stage_insert(new_id, new, Some(old));
        let mut closed = current;
        closed.valid_to = Some(closed_at);
        closed.superseded_by = Some(new_id);
        staged.overlay.insert(old, closed);
        staged.ops.push(Op::Close {
            id: old,
            at: closed_at,
            by: new_id,
            actor,
        });
        Ok(new_id)
    }

    async fn set_status(
        &self,
        tx: &mut MemoryTx,
        id: RecordId,
        to: Status,
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<()> {
        let staged = staged(tx)?;
        let mut record = self
            .visible(staged, id)
            .await?
            .ok_or(MemoryError::NotFound(id))?;
        if !record.status.can_transition_to(to) {
            return Err(MemoryError::InvalidTransition {
                from: record.status,
                to,
            });
        }
        let from = record.status;
        record.status = to;
        match to {
            Status::Active => record.approved_at = Some(at),
            Status::Retracted if record.valid_to.is_none() => record.valid_to = Some(at),
            _ => {}
        }
        staged.overlay.insert(id, record);
        staged.ops.push(Op::Status {
            id,
            from,
            to,
            actor,
            at,
        });
        Ok(())
    }

    async fn set_core(
        &self,
        tx: &mut MemoryTx,
        id: RecordId,
        pin: Option<CorePin>,
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<()> {
        let (core, order, max_tokens) = match pin {
            Some(pin) => (true, pin.order, Some(pin.max_tokens)),
            None => (false, None, None),
        };
        let staged = staged(tx)?;
        let mut record = self
            .visible(staged, id)
            .await?
            .ok_or(MemoryError::NotFound(id))?;
        if record.status != Status::Active || record.valid_to.is_some() {
            return Err(MemoryError::InvalidInput(
                "only a current active record can be pinned or unpinned".to_owned(),
            ));
        }
        record.is_core = core;
        record.core_order = order;
        staged.overlay.insert(id, record);
        staged.ops.push(Op::Core {
            id,
            core,
            order,
            max_tokens,
            actor,
            at,
        });
        Ok(())
    }

    async fn record_use(&self, ids: &[RecordId], at: Timestamp) -> MemoryResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        write(&self.store, move |conn| {
            for id in &ids {
                conn.execute(
                    "UPDATE memory_records SET last_used_at=?1, use_count=use_count+1 WHERE id=?2",
                    params![at.0, id],
                )?;
            }
            Ok(())
        })
        .await
    }

    async fn get(&self, id: RecordId) -> MemoryResult<Option<Record>> {
        self.committed(id).await
    }

    async fn list(&self, filter: RecordFilter, page: Page) -> MemoryResult<Vec<Record>> {
        let (sql, args) = list_sql(&filter, page);
        read(&self.store, move |conn| {
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(args), record_from)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn core_block(&self, scopes: &[Scope]) -> MemoryResult<Vec<Record>> {
        if scopes.is_empty() {
            return Ok(Vec::new());
        }
        let (sql, args) = core_sql(scopes);
        read(&self.store, move |conn| {
            let mut statement = conn.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(args), record_from)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn purge(
        &self,
        ids: &[RecordId],
        actor: Actor,
        at: Timestamp,
    ) -> MemoryResult<PurgeReport> {
        let ids = ids.to_vec();
        write_secure(&self.store, move |conn| {
            memory_purge::purge_records(conn, &ids, actor, at)
        })
        .await
    }

    async fn events(&self, id: RecordId) -> MemoryResult<Vec<MemoryEvent>> {
        read(&self.store, move |conn| {
            let mut statement = conn.prepare(
                "SELECT seq,record_id,event,actor,detail,at FROM memory_events \
                 WHERE record_id=? ORDER BY seq",
            )?;
            let rows = statement.query_map([id.to_string()], event_from)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}

impl std::fmt::Debug for SqliteMemoryRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteMemoryRepository")
            .finish_non_exhaustive()
    }
}
